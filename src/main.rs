use serde::Deserialize;
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

const DEFAULT_QUOTA: u64 = 250_000;
const DEFAULT_RESERVE: u64 = 50_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Auto,
    Api,
    Chatgpt,
}

#[derive(Debug)]
struct Cli {
    mode: Mode,
    status_only: bool,
    dry_run: bool,
    quota: Option<u64>,
    reserve: Option<u64>,
    codex_args: Vec<OsString>,
}

#[derive(Debug)]
struct Config {
    admin_key: Option<String>,
    project_id: Option<String>,
    api_home: PathBuf,
    chatgpt_home: PathBuf,
    codex_bin: OsString,
    quota: u64,
    reserve: u64,
    models: Vec<String>,
    quiet: bool,
}

#[derive(Debug, Deserialize)]
struct UsagePage {
    #[serde(default)]
    data: Vec<UsageBucket>,
    #[serde(default)]
    has_more: bool,
    next_page: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageBucket {
    #[serde(default)]
    results: Vec<UsageResult>,
}

#[derive(Debug, Deserialize)]
struct UsageResult {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug)]
struct Decision {
    used: Option<u64>,
    cutoff: u64,
    selected: Mode,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("codex-smart: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8, String> {
    let cli = parse_cli(env::args_os().skip(1))?;
    let config = Config::from_env(&cli)?;
    let decision = decide(&cli, &config);

    match decision {
        Ok(decision) => {
            print_decision(&config, &decision);
            if cli.status_only || cli.dry_run {
                return Ok(0);
            }
            launch_codex(&config, decision.selected, &cli.codex_args)
        }
        Err(error) if cli.mode == Mode::Auto => {
            if !config.quiet {
                eprintln!(
                    "codex-smart: usage check unavailable ({error}); falling back to ChatGPT auth"
                );
            }
            if cli.status_only || cli.dry_run {
                return Ok(0);
            }
            launch_codex(&config, Mode::Chatgpt, &cli.codex_args)
        }
        Err(error) => Err(error),
    }
}

impl Config {
    fn from_env(cli: &Cli) -> Result<Self, String> {
        let quota = cli
            .quota
            .or(env_u64("CODEX_SMART_QUOTA_TOKENS")?)
            .unwrap_or(DEFAULT_QUOTA);
        let reserve = cli
            .reserve
            .or(env_u64("CODEX_SMART_RESERVE_TOKENS")?)
            .unwrap_or(DEFAULT_RESERVE);

        let models = env::var("CODEX_SMART_MODELS")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let chatgpt_home = env::var_os("CODEX_SMART_CHATGPT_HOME")
            .or_else(|| env::var_os("CODEX_HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|| default_codex_home(".codex"));
        let api_home = env::var_os("CODEX_SMART_API_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_codex_home(".codex-api-free"));

        Ok(Self {
            admin_key: env::var("OPENAI_ADMIN_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            project_id: env::var("OPENAI_PROJECT_ID")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            api_home,
            chatgpt_home,
            codex_bin: env::var_os("CODEX_SMART_CODEX_BIN")
                .unwrap_or_else(|| OsString::from("codex")),
            quota,
            reserve,
            models,
            quiet: env_truthy("CODEX_SMART_QUIET"),
        })
    }
}

fn decide(cli: &Cli, config: &Config) -> Result<Decision, String> {
    let cutoff = config.quota.saturating_sub(config.reserve);

    match cli.mode {
        Mode::Api => Ok(Decision {
            used: None,
            cutoff,
            selected: Mode::Api,
        }),
        Mode::Chatgpt => Ok(Decision {
            used: None,
            cutoff,
            selected: Mode::Chatgpt,
        }),
        Mode::Auto => {
            let admin_key = config.admin_key.as_deref().ok_or_else(|| {
                "OPENAI_ADMIN_KEY is required for auto mode; refusing API use".to_string()
            })?;
            let project_id = config.project_id.as_deref().ok_or_else(|| {
                "OPENAI_PROJECT_ID is required for auto mode; refusing API use".to_string()
            })?;
            let used = fetch_usage(admin_key, project_id, &config.models)?;
            let selected = if used < cutoff {
                Mode::Api
            } else {
                Mode::Chatgpt
            };
            Ok(Decision {
                used: Some(used),
                cutoff,
                selected,
            })
        }
    }
}

fn fetch_usage(admin_key: &str, project_id: &str, models: &[String]) -> Result<u64, String> {
    let now = unix_now()?;
    let start = now - (now % 86_400);
    let mut page: Option<String> = None;
    let mut total = 0_u64;

    loop {
        let mut url = Url::parse("https://api.openai.com/v1/organization/usage/completions")
            .map_err(|error| format!("invalid Usage API URL: {error}"))?;

        {
            let mut query = url.query_pairs_mut();
            query.append_pair("start_time", &start.to_string());
            query.append_pair("end_time", &(now + 1).to_string());
            query.append_pair("bucket_width", "1d");
            query.append_pair("limit", "1");
            query.append_pair("project_ids[]", project_id);
            for model in models {
                query.append_pair("models[]", model);
            }
            if let Some(cursor) = page.as_deref() {
                query.append_pair("page", cursor);
            }
        }

        let response = ureq::get(url.as_str())
            .set("Authorization", &format!("Bearer {admin_key}"))
            .set("Accept", "application/json")
            .call()
            .map_err(format_ureq_error)?;

        let payload: UsagePage = response
            .into_json()
            .map_err(|error| format!("failed to decode Usage API response: {error}"))?;

        for bucket in payload.data {
            for result in bucket.results {
                total = total
                    .checked_add(result.input_tokens)
                    .and_then(|value| value.checked_add(result.output_tokens))
                    .ok_or_else(|| "token usage overflow".to_string())?;
            }
        }

        if !payload.has_more {
            break;
        }
        page = payload.next_page;
        if page.is_none() {
            return Err("Usage API returned has_more=true without next_page".to_string());
        }
    }

    Ok(total)
}

fn format_ureq_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            if body.is_empty() {
                format!("Usage API returned HTTP {code}")
            } else {
                format!("Usage API returned HTTP {code}: {}", truncate(&body, 500))
            }
        }
        ureq::Error::Transport(error) => format!("Usage API transport error: {error}"),
    }
}

fn launch_codex(config: &Config, mode: Mode, args: &[OsString]) -> Result<u8, String> {
    let home = match mode {
        Mode::Api => &config.api_home,
        Mode::Chatgpt => &config.chatgpt_home,
        Mode::Auto => return Err("internal error: unresolved auto mode".to_string()),
    };

    let mut command = Command::new(&config.codex_bin);
    command.args(args);
    command.env("CODEX_HOME", home);

    // Keep routing credentials out of the child process. Removing API-key
    // overrides also ensures the selected CODEX_HOME/auth.json is authoritative.
    command.env_remove("OPENAI_ADMIN_KEY");
    command.env_remove("OPENAI_PROJECT_ID");
    command.env_remove("OPENAI_API_KEY");
    command.env_remove("CODEX_API_KEY");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(format!("failed to exec {:?}: {error}", config.codex_bin))
    }

    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .map_err(|error| format!("failed to launch {:?}: {error}", config.codex_bin))?;
        Ok(status.code().unwrap_or(1).clamp(0, 255) as u8)
    }
}

fn print_decision(config: &Config, decision: &Decision) {
    if config.quiet {
        return;
    }

    let selected = mode_name(decision.selected);
    match decision.used {
        Some(used) => eprintln!(
            "codex-smart: used={used} quota={} reserve={} cutoff={} -> {selected}",
            config.quota, config.reserve, decision.cutoff
        ),
        None => eprintln!(
            "codex-smart: forced route -> {selected} (quota={} reserve={} cutoff={})",
            config.quota, config.reserve, decision.cutoff
        ),
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Auto => "auto",
        Mode::Api => "api",
        Mode::Chatgpt => "chatgpt",
    }
}

fn parse_cli<I>(args: I) -> Result<Cli, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut mode = Mode::Auto;
    let mut status_only = false;
    let mut dry_run = false;
    let mut quota = None;
    let mut reserve = None;
    let mut codex_args = Vec::new();
    let mut iter = args.into_iter();
    let mut passthrough = false;

    while let Some(arg) = iter.next() {
        if passthrough {
            codex_args.push(arg);
            continue;
        }
        if arg == "--" {
            passthrough = true;
            continue;
        }

        let text = arg.to_string_lossy();
        if text == "--smart-status" {
            status_only = true;
        } else if text == "--smart-dry-run" {
            dry_run = true;
        } else if text == "--smart-help" {
            print_help();
            std::process::exit(0);
        } else if text == "--smart-mode" {
            let value = iter
                .next()
                .ok_or_else(|| "--smart-mode requires auto, api, or chatgpt".to_string())?;
            mode = parse_mode(&value.to_string_lossy())?;
        } else if let Some(value) = text.strip_prefix("--smart-mode=") {
            mode = parse_mode(value)?;
        } else if text == "--smart-quota" {
            quota = Some(parse_next_u64("--smart-quota", iter.next())?);
        } else if let Some(value) = text.strip_prefix("--smart-quota=") {
            quota = Some(parse_u64("--smart-quota", value)?);
        } else if text == "--smart-reserve" {
            reserve = Some(parse_next_u64("--smart-reserve", iter.next())?);
        } else if let Some(value) = text.strip_prefix("--smart-reserve=") {
            reserve = Some(parse_u64("--smart-reserve", value)?);
        } else {
            codex_args.push(arg);
        }
    }

    Ok(Cli {
        mode,
        status_only,
        dry_run,
        quota,
        reserve,
        codex_args,
    })
}

fn parse_mode(value: &str) -> Result<Mode, String> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => Ok(Mode::Auto),
        "api" => Ok(Mode::Api),
        "chatgpt" | "subscription" => Ok(Mode::Chatgpt),
        _ => Err(format!(
            "invalid --smart-mode={value:?}; expected auto, api, or chatgpt"
        )),
    }
}

fn parse_next_u64(flag: &str, value: Option<OsString>) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("{flag} requires an integer"))?;
    parse_u64(flag, &value.to_string_lossy())
}

fn parse_u64(flag: &str, value: &str) -> Result<u64, String> {
    value
        .replace('_', "")
        .parse::<u64>()
        .map_err(|_| format!("{flag} requires a non-negative integer, got {value:?}"))
}

fn env_u64(name: &str) -> Result<Option<u64>, String> {
    match env::var(name) {
        Ok(value) => parse_u64(name, &value).map(Some),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("failed to read {name}: {error}")),
    }
}

fn env_truthy(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))
}

fn default_codex_home(name: &str) -> PathBuf {
    home_dir().unwrap_or_else(|| PathBuf::from(".")).join(name)
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        env::var_os("HOME").map(PathBuf::from)
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn print_help() {
    println!(
        r#"codex-smart - usage-aware Codex launcher

Usage:
  codex-smart [smart options] [--] [codex arguments...]

Smart options:
  --smart-mode <auto|api|chatgpt>  Override route (default: auto)
  --smart-status                   Show the route decision without launching Codex
  --smart-dry-run                  Do not launch Codex
  --smart-quota <tokens>           Override daily complimentary-token quota
  --smart-reserve <tokens>         Stop API use this many tokens before quota
  --smart-help                     Show this help

Environment:
  OPENAI_ADMIN_KEY                 Admin key used only for Usage API lookup
  OPENAI_PROJECT_ID                Dedicated API project to count
  CODEX_SMART_API_HOME             API-key CODEX_HOME (default: ~/.codex-api-free)
  CODEX_SMART_CHATGPT_HOME         ChatGPT CODEX_HOME (default: $CODEX_HOME or ~/.codex)
  CODEX_SMART_QUOTA_TOKENS         Default 250000
  CODEX_SMART_RESERVE_TOKENS       Default 50000
  CODEX_SMART_MODELS               Optional comma-separated model filter
  CODEX_SMART_CODEX_BIN            Codex executable (default: codex)
  CODEX_SMART_QUIET                1/true/yes/on to suppress route message

Auto mode fails closed: if Usage API lookup fails or required settings are
missing, Codex launches with ChatGPT authentication rather than API auth.
"#
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_passthrough_args() {
        let cli = parse_cli([
            OsString::from("--smart-mode=api"),
            OsString::from("exec"),
            OsString::from("--json"),
        ])
        .unwrap();
        assert_eq!(cli.mode, Mode::Api);
        assert_eq!(cli.codex_args, vec!["exec", "--json"]);
    }

    #[test]
    fn parses_underscored_numbers() {
        let cli = parse_cli([
            OsString::from("--smart-quota=250_000"),
            OsString::from("--smart-reserve"),
            OsString::from("50_000"),
        ])
        .unwrap();
        assert_eq!(cli.quota, Some(250_000));
        assert_eq!(cli.reserve, Some(50_000));
    }

    #[test]
    fn cutoff_saturates() {
        assert_eq!(100_u64.saturating_sub(200), 0);
    }

    #[test]
    fn utc_midnight_math_is_epoch_aligned() {
        let now = 86_400 * 20_000 + 12_345;
        let start = now - now % 86_400;
        assert_eq!(start % 86_400, 0);
        assert!(start <= now);
    }
}
