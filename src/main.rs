use anyhow::{Context, Result};
use clipboard::{ClipboardContext, ClipboardProvider};
use difference::{Changeset, Difference};
use env_logger::Builder as EnvLoggerBuilder;
use log::info;
use log::warn;
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use std::char;
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;
use thiserror::Error;

const DEFAULT_REPLACEMENTS: &str = include_str!("default_replacements.json");
const DEFAULT_EXCLUSIONS: &str = include_str!("default_exclusions.json");
const REPLACEMENTS_FILE_NAME: &str = "replacements.json";
const EXCLUSIONS_FILE_NAME: &str = "exclusions.json";

#[derive(Debug, serde::Deserialize, Hash)]
struct Replacement {
    original: String,
    replacement: String,
}

#[derive(Debug, serde::Deserialize)]
struct Exclusions {
    exclude: Vec<char>,
}

#[derive(Debug, Error)]
enum ClipboardError {
    #[error("Failed to create clipboard provider: {0}")]
    CreateContext(String),
    #[error("Failed to set clipboard contents: {0}")]
    SetContents(String),
    #[error("Failed to get clipboard contents: {0}")]
    GetContents(String),
}

fn calculate_hash<T: Hash>(t: &T) -> u64 {
    let mut s = DefaultHasher::new();
    t.hash(&mut s);
    s.finish()
}

fn get_config_dir() -> Result<PathBuf> {
    let config_dir = if let Some(config_dir) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(config_dir)
    } else {
        dirs::config_dir().context("Failed to get config directory")?
    };
    Ok(config_dir.join("kill-zen-all"))
}

fn create_default_config() -> Result<()> {
    let config_dir = get_config_dir()?;
    if !config_dir.exists() {
        fs::create_dir_all(&config_dir).context("Failed to create config directory")?;
    }
    let replacement_path = config_dir.join(REPLACEMENTS_FILE_NAME);
    if !replacement_path.exists() {
        let default_replacements = DEFAULT_REPLACEMENTS;
        fs::write(&replacement_path, default_replacements)
            .context("Failed to create default replacements file")?;
        info!(
            "Created default replacements file: {}",
            replacement_path
                .to_str()
                .context("Failed to convert path to string")?
        );
    }
    let exclusion_path = config_dir.join(EXCLUSIONS_FILE_NAME);
    if !exclusion_path.exists() {
        let default_exclusions = DEFAULT_EXCLUSIONS;
        fs::write(&exclusion_path, default_exclusions)
            .context("Failed to create default exclusions file")?;
        info!(
            "Created default exclusions file: {}",
            exclusion_path
                .to_str()
                .context("Failed to convert path to string")?
        );
    }
    Ok(())
}

fn load_json<T>(file_path: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let data = fs::read_to_string(file_path)?;
    serde_json::from_str(&data).context("Failed to parse JSON")
}

fn load_replacements(file_path: &str) -> Result<Vec<Replacement>> {
    load_json::<Vec<Replacement>>(file_path).context("Failed to load replacements")
}

fn load_exclusion_list(file_path: &str) -> Result<Vec<char>> {
    let exclusions: Exclusions = load_json(file_path)?;
    Ok(exclusions.exclude)
}

fn format_text(
    text: &str,
    replacements: &[Replacement],
    exclusion_list: &[char],
) -> Result<String> {
    let mut formatted_content = text.to_string();
    for replacement in replacements {
        formatted_content =
            formatted_content.replace(&replacement.original, &replacement.replacement);
    }
    let re = Regex::new(r"[！-～]").context("Failed to create regex pattern")?;
    formatted_content = re
        .replace_all(&formatted_content, |caps: &regex::Captures| {
            let c = caps[0].chars().next().unwrap_or_default();
            if exclusion_list.contains(&c) {
                c.to_string()
            } else {
                let half_width_char = (c as u32 - 0xfee0) as u8 as char;
                half_width_char.to_string()
            }
        })
        .to_string();
    Ok(formatted_content)
}

fn create_clipboard_context() -> Result<ClipboardContext, ClipboardError> {
    let mut ctx =
        ClipboardContext::new().map_err(|e| ClipboardError::CreateContext(e.to_string()))?;
    if ctx.get_contents().is_err() && ctx.set_contents("".to_string()).is_err() {
        return Err(ClipboardError::CreateContext(
            "Failed to set empty contents".to_string(),
        ));
    };
    Ok(ctx)
}

fn set_clipboard_contents(
    ctx: &mut ClipboardContext,
    content: String,
) -> Result<(), ClipboardError> {
    ctx.set_contents(content)
        .map_err(|e| ClipboardError::SetContents(e.to_string()))?;
    Ok(())
}

fn get_clipboard_contents(ctx: &mut ClipboardContext) -> Result<String, ClipboardError> {
    ctx.get_contents()
        .map_err(|e| ClipboardError::GetContents(e.to_string()))
}

fn highlight_diff(original: &str, formatted: &str) -> String {
    let changeset = Changeset::new(original, formatted, "");
    let mut highlighted = String::new();
    for change in changeset.diffs {
        match change {
            Difference::Same(s) => highlighted.push_str(&s),
            Difference::Add(s) => highlighted.push_str(&format!("\x1b[32m{}\x1b[0m", s)),
            Difference::Rem(s) => highlighted.push_str(&format!("\x1b[31m{}\x1b[0m", s)),
        }
    }
    highlighted
}

fn main() -> Result<()> {
    EnvLoggerBuilder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    create_default_config()?;

    let replacement_path = get_config_dir()?.join(REPLACEMENTS_FILE_NAME);
    let exclusion_path = get_config_dir()?.join(EXCLUSIONS_FILE_NAME);

    let mut replacements = load_replacements(
        replacement_path
            .to_str()
            .context("Replacement path contains invalid UTF-8 characters")?,
    )?;
    let mut exclusion_list = load_exclusion_list(
        exclusion_path
            .to_str()
            .context("Exclusion path contains invalid UTF-8 characters")?,
    )?;
    let (tx, rx) = channel();
    let config = Config::default().with_poll_interval(Duration::from_secs(2));
    let mut watcher: RecommendedWatcher =
        Watcher::new(tx.clone(), config).context("Failed to initialize file watcher")?;
    watcher
        .watch(&replacement_path, RecursiveMode::NonRecursive)
        .context("Failed to watch replacements file")?;
    watcher
        .watch(&exclusion_path, RecursiveMode::NonRecursive)
        .context("Failed to watch exclusions file")?;
    let mut ctx: ClipboardContext =
        create_clipboard_context().context("Failed to create context")?;

    let mut previous_replacement_hash = calculate_hash(&replacements);
    let mut previous_exclusion_hash = calculate_hash(&exclusion_list);

    let mut replacement_failed = false;
    let mut exclusion_failed = false;

    loop {
        // Get clipboard content with better error handling
        let clipboard_result = get_clipboard_contents(&mut ctx);

        if let Ok(clipboard_content) = clipboard_result {
            // Only process if we successfully got clipboard content
            if let Ok(formatted_content) =
                format_text(&clipboard_content, &replacements, &exclusion_list)
            {
                if clipboard_content != formatted_content {
                    info!(
                        "Formatted\n{}",
                        highlight_diff(&clipboard_content, &formatted_content)
                    );
                    // Don't crash if setting clipboard fails
                    if let Err(e) = set_clipboard_contents(&mut ctx, formatted_content) {
                        warn!("Failed to set clipboard contents: {}", e);
                    }
                }
            } else {
                warn!("Failed to format clipboard text");
            }
        } else {
            // If clipboard access fails, recreate the clipboard context
            warn!("Failed to get clipboard contents, attempting to recreate context");
            match create_clipboard_context() {
                Ok(new_ctx) => {
                    ctx = new_ctx;
                    info!("Successfully recreated clipboard context");
                }
                Err(e) => {
                    warn!("Failed to recreate clipboard context: {}", e);
                    // Wait a bit longer before retrying when clipboard is inaccessible
                    thread::sleep(Duration::from_secs(3));
                }
            }
        }

        // Check for file changes
        match rx.try_recv() {
            Ok(events) => {
                for event in events.iter() {
                    if event.paths.contains(&replacement_path) {
                        let Ok(new_replacements) = load_replacements(
                            replacement_path
                                .to_str()
                                .context("Failed to convert path to string")?,
                        ) else {
                            if !replacement_failed {
                                warn!("Failed to load replacements.")
                            };
                            replacement_failed = true;
                            continue;
                        };
                        let new_replacement_hash = calculate_hash(&new_replacements);
                        if previous_replacement_hash != new_replacement_hash {
                            info!("{} has been modified.", REPLACEMENTS_FILE_NAME);
                            info!("Reloading replacements...");
                            replacements = new_replacements;
                            previous_replacement_hash = new_replacement_hash;
                            replacement_failed = false;
                        }
                    }
                    if event.paths.contains(&exclusion_path) {
                        let Ok(new_exclusion_list) = load_exclusion_list(
                            exclusion_path
                                .to_str()
                                .context("Failed to convert path to string")?,
                        ) else {
                            if !exclusion_failed {
                                warn!("Failed to load exclusions.");
                            }
                            exclusion_failed = true;
                            continue;
                        };
                        let new_exclusion_hash = calculate_hash(&new_exclusion_list);
                        if previous_exclusion_hash != new_exclusion_hash {
                            info!("{} has been modified.", EXCLUSIONS_FILE_NAME);
                            info!("Reloading exclusions...");
                            exclusion_list = new_exclusion_list;
                            previous_exclusion_hash = new_exclusion_hash;
                            exclusion_failed = false;
                        }
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // No events, continue normally
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                warn!("File watcher disconnected, attempting to reconnect");
                // Try to recreate the watcher
                match Watcher::new(tx.clone(), config) {
                    Ok(new_watcher) => {
                        watcher = new_watcher;
                        // Re-watch the files
                        if let Err(e) =
                            watcher.watch(&replacement_path, RecursiveMode::NonRecursive)
                        {
                            warn!("Failed to watch replacements file: {}", e);
                        }
                        if let Err(e) = watcher.watch(&exclusion_path, RecursiveMode::NonRecursive)
                        {
                            warn!("Failed to watch exclusions file: {}", e);
                        }
                        info!("Successfully reconnected file watcher");
                    }
                    Err(e) => {
                        warn!("Failed to recreate file watcher: {}", e);
                    }
                }
            }
        }

        // Sleep to prevent high CPU usage
        thread::sleep(Duration::from_secs(1));
    }
}

// Test code
#[cfg(test)]
mod tests {
    use super::*;

    // Test for get_config_dir
    use std::env;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_create_default_config() {
        // 一時ディレクトリを作成
        let temp_dir = tempdir().unwrap();
        let temp_path = temp_dir.path().to_path_buf();

        // 一時ディレクトリをXDG_CONFIG_HOMEに設定
        env::set_var("XDG_CONFIG_HOME", &temp_path);

        // create_default_config()を呼び出す
        create_default_config().unwrap();

        // 設定ファイルが正しい場所に作成されたかを確認
        let replacements_path = temp_path.join("kill-zen-all").join("replacements.json");
        let exclusions_path = temp_path.join("kill-zen-all").join("exclusions.json");

        assert!(
            replacements_path.exists(),
            "replacements.json が存在しません"
        );
        assert!(exclusions_path.exists(), "exclusions.json が存在しません");

        // replacements.json の内容を検証
        let replacements_content =
            fs::read_to_string(&replacements_path).expect("Failed to read replacements.json");
        let expected_replacements_content = r#"[
  { "original": "，", "replacement": ", " },
  { "original": "．", "replacement": ". " },
  { "original": "CRLF", "replacement": "。" },
  { "original": "頚", "replacement": "頸" }
]"#
        .trim(); // テスト用に改行とインデントを除去

        assert_eq!(replacements_content.trim(), expected_replacements_content);

        // exclusions.json の内容を検証
        let exclusions_content =
            fs::read_to_string(&exclusions_path).expect("Failed to read exclusions.json");
        let expected_exclusions_content = r#"{
  "exclude": ["　", "！", "？", "〜", "～"]
}"#
        .trim(); // テスト用に改行とインデントを除去

        assert_eq!(exclusions_content.trim(), expected_exclusions_content);

        // 環境変数のクリーンアップ
        env::remove_var("XDG_CONFIG_HOME");
    }
    // Test for load_replacements
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_load_replacements() {
        let test_data = r#"
        [
            {"original": "foo", "replacement": "bar"},
            {"original": "baz", "replacement": "qux"}
        ]
        "#;

        // Create a test file
        let file_path = "test_replacements.json";
        let mut file = File::create(file_path).unwrap();
        file.write_all(test_data.as_bytes()).unwrap();

        let replacements = load_replacements(file_path).unwrap();
        assert_eq!(replacements.len(), 2);
        assert_eq!(replacements[0].original, "foo");
        assert_eq!(replacements[0].replacement, "bar");
        assert_eq!(replacements[1].original, "baz");
        assert_eq!(replacements[1].replacement, "qux");

        // Remove the test file
        fs::remove_file(file_path).unwrap();
    }

    // Test for load_replacements with nonexistent file
    #[test]
    fn test_load_replacements_no_file() {
        let file_path = "nonexistent.json";
        let result = load_replacements(file_path);
        assert!(result.is_err());
    }

    // Test for load_replacements with invalid JSON
    #[test]
    fn test_load_replacements_invalid_json() {
        let test_data = r#"
        [
            {"original": "foo", "replacement": "bar"},
            {"original": "baz", "replacement": "qux"}
        "#;

        // Create a test file
        let file_path = "test_invalid_replacements.json";
        let mut file = File::create(file_path).unwrap();
        file.write_all(test_data.as_bytes()).unwrap();

        let result = load_replacements(file_path);
        println!("{:?}", result);
        assert!(result.is_err());

        // Remove the test file
        fs::remove_file(file_path).unwrap();
    }

    // Test for load_exclusion_list
    #[test]
    fn test_load_exclusion_list() {
        let test_data = r#"
        {
            "exclude": ["！", "？"]
        }
        "#;

        // Create a test file
        let file_path = "test_exclusions.json";
        let mut file = File::create(file_path).unwrap();
        file.write_all(test_data.as_bytes()).unwrap();

        let exclusions = load_exclusion_list(file_path).unwrap();
        assert_eq!(exclusions.len(), 2);
        assert_eq!(exclusions[0], '！');
        assert_eq!(exclusions[1], '？');

        // Remove the test file
        fs::remove_file(file_path).unwrap();
    }

    // Test for load_exclusion_list with nonexistent file
    #[test]
    fn test_load_exclusion_list_no_file() {
        let file_path = "nonexistent.json";
        let result = load_exclusion_list(file_path);
        assert!(result.is_err());
    }

    // Test for load_exclusion_list with invalid JSON
    #[test]
    fn test_load_exclusion_list_invalid_json() {
        let test_data = r#"
        {
            "exclude": ["！", "？
        }
        "#;

        // Create a test file
        let file_path = "test_invalid_exclusions.json";
        let mut file = File::create(file_path).unwrap();
        file.write_all(test_data.as_bytes()).unwrap();

        let result = load_exclusion_list(file_path);
        assert!(result.is_err());

        // Remove the test file
        fs::remove_file(file_path).unwrap();
    }

    // Test for format_text
    #[test]
    fn test_format_text_with_replacements_exclusions() {
        // 置換リスト
        let replacements = vec![
            Replacement {
                original: "foo".to_string(),
                replacement: "bar".to_string(),
            },
            Replacement {
                original: "baz".to_string(),
                replacement: "qux".to_string(),
            },
        ];

        // 除外リスト
        let exclusion_list = vec!['！', '？']; // 例: 全角の「！」「？」を除外

        // テストケース
        let input = "foo baz １２３４！？";
        let expected = "bar qux 1234！？"; // ！？は除外されるので変換されない
        let formatted = format_text(input, &replacements, &exclusion_list).unwrap();

        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_text_with_replacements_without_exclusions() {
        // 置換リスト
        let replacements = vec![
            Replacement {
                original: "foo".to_string(),
                replacement: "bar".to_string(),
            },
            Replacement {
                original: "baz".to_string(),
                replacement: "qux".to_string(),
            },
        ];

        // 除外リストなし
        let exclusion_list = vec![];

        // テストケース
        let input = "foo baz １２３４！？";
        let expected = "bar qux 1234!?"; // 全ての文字が変換される
        let formatted = format_text(input, &replacements, &exclusion_list).unwrap();

        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_text_without_replacements_with_exclusions() {
        // 置換リストなし
        let replacements = vec![];

        // 除外リスト
        let exclusion_list = vec!['！', '？']; // 例: 全角の「！」「？」を除外

        // テストケース
        let input = "foo baz １２３４！？";
        let expected = "foo baz 1234！？"; // ！？は除外されるので変換されない
        let formatted = format_text(input, &replacements, &exclusion_list).unwrap();

        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_text_without_replacements_exclusions() {
        // 置換リストなし
        let replacements = vec![];

        // 除外リストなし
        let exclusion_list = vec![];

        // テストケース
        let input = "foo baz １２３４！？";
        let expected = "foo baz 1234!?"; // 全ての文字が変換される
        let formatted = format_text(input, &replacements, &exclusion_list).unwrap();

        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_text_with_partial_exclusions() {
        // 置換リスト
        let replacements = vec![
            Replacement {
                original: "foo".to_string(),
                replacement: "bar".to_string(),
            },
            Replacement {
                original: "baz".to_string(),
                replacement: "qux".to_string(),
            },
        ];

        // 部分的な除外リスト
        let exclusion_list = vec!['！']; // 例: 全角の「！」を除外

        // テストケース
        let input = "foo baz １２３４！？";
        let expected = "bar qux 1234！?"; // ！は変換されず、？は変換される
        let formatted = format_text(input, &replacements, &exclusion_list).unwrap();

        assert_eq!(formatted, expected);
    }

    // Test for highlight_diff
    #[test]
    fn test_diff_no_changes() {
        let original = "This is a test.";
        let formatted = "This is a test.";
        let result = highlight_diff(original, formatted);
        // 差分がない場合はそのままの文字列が返るはず
        assert_eq!(result, "This is a test.");
    }

    #[test]
    fn test_diff_with_addition() {
        let original = "This is a test";
        let formatted = "This is a test!";
        let result = highlight_diff(original, formatted);
        // 追加された「!」が緑色（ANSIエスケープシーケンスで囲まれている）で表示される
        let expected = "This is a test\x1b[32m!\x1b[0m";
        assert_eq!(result, expected);
    }

    #[test]
    fn test_diff_with_removal() {
        let original = "This is a test!";
        let formatted = "This is a test";
        let result = highlight_diff(original, formatted);
        // 削除された「!」が赤色で表示される
        let expected = "This is a test\x1b[31m!\x1b[0m";
        assert_eq!(result, expected);
    }

    #[test]
    fn test_diff_with_complex_changes() {
        let original = "A string";
        let formatted = "B string";
        let result = highlight_diff(original, formatted);
        // 変更された削除された'A'が赤色、追加された'B'が緑色で表示される
        let expected = "\x1b[31mA\x1b[0m\x1b[32mB\x1b[0m string";
        assert_eq!(result, expected);
    }

    // Test for kill-zen-all
    use clipboard::{ClipboardContext, ClipboardProvider};

    #[test]
    fn test_clipboard_integration() {
        if std::env::var("CI").is_ok() {
            // CI環境ではクリップボードを操作できないのでスキップ
            // このテストはローカル環境でのみ実行される
            return;
        }
        let mut ctx: ClipboardContext = ClipboardProvider::new().unwrap();
        let original_text = "foo baz １２３４！";
        ctx.set_contents(original_text.to_string()).unwrap();

        let replacements = vec![
            Replacement {
                original: "foo".to_string(),
                replacement: "bar".to_string(),
            },
            Replacement {
                original: "baz".to_string(),
                replacement: "qux".to_string(),
            },
        ];
        let exclusion_list = vec![];

        let clipboard_content = ctx.get_contents().unwrap();
        let formatted_content =
            format_text(&clipboard_content, &replacements, &exclusion_list).unwrap();
        ctx.set_contents(formatted_content.clone()).unwrap();

        assert_eq!(formatted_content, "bar qux 1234!");
        assert_eq!(ctx.get_contents().unwrap(), "bar qux 1234!");
    }
}
