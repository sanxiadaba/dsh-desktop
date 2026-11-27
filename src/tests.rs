//! 不需要桌面的单元测试：解析、映射、纯计算、文档漂移。

use crate::cli::Args;
use crate::input;
use crate::uia;

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn args_parses_flags_and_values() {
    let a = Args::new(&argv(&["--window", "w-1a", "--depth=7", "--raw", "target"]));
    assert_eq!(a.opt("--window").as_deref(), Some("w-1a"));
    assert_eq!(a.opt("--depth").as_deref(), Some("7"));
    assert!(a.has("--raw"));
    assert_eq!(a.positionals(), vec!["target".to_string()]);
}

#[test]
fn args_positionals_skip_option_values() {
    let a = Args::new(&argv(&["--window", "w-1a", "@s1:e2"]));
    assert_eq!(a.positionals(), vec!["@s1:e2".to_string()]);
}

#[test]
fn key_combo_parses_modifiers_and_key() {
    let (mods, vk) = input::parse_combo("ctrl+shift+s").unwrap();
    assert_eq!(vk, 'S' as u16);
    assert_eq!(mods.len(), 2);
}

#[test]
fn key_combo_rejects_two_main_keys() {
    let err = input::parse_combo("ctrl+a+b").unwrap_err();
    assert_eq!(err.code, "BAD_ARGS");
}

#[test]
fn key_names_map_to_virtual_keys() {
    assert_eq!(input::parse_key("enter").unwrap(), 0x0D);
    assert_eq!(input::parse_key("f5").unwrap(), 0x74);
    // 标点键走 VkKeyScanW，不再只认字母数字。
    assert!(input::parse_key("/").is_ok());
    assert!(input::parse_key("no-such-key").is_err());
}

#[test]
fn roles_map_to_stable_tokens() {
    assert_eq!(uia::role_name(50000), "button");
    assert_eq!(uia::role_name(50013), "radio");
    assert_eq!(uia::role_name(50019), "tabitem");
    assert_eq!(uia::role_name(12345), "unknown");
}

#[test]
fn admin_gate_comes_from_the_command_table() {
    for ok in ["help", "status", "elevate"] {
        assert!(!crate::commands::needs_admin(ok), "{ok} 应该允许非管理员");
    }
    for deny in ["click", "type", "snap", "windows", "shot", "run", "doctor"] {
        assert!(crate::commands::needs_admin(deny), "{deny} 应该要求管理员");
    }
    // 表里没有的命令按"需要管理员"处理，并会被 dispatch 拒绝
    assert!(crate::commands::needs_admin("no-such-command"));
}

#[test]
fn command_names_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for spec in crate::commands::COMMANDS {
        assert!(seen.insert(spec.name), "命令重复：{}", spec.name);
    }
}

#[test]
fn every_command_is_in_the_help_text() {
    let help = crate::cli::HELP_MAIN;
    for spec in crate::commands::COMMANDS {
        if spec.name == "focus" {
            continue; // 已禁用，帮助里不再列
        }
        assert!(
            help.contains(spec.name),
            "命令 {} 没写进 help（会让人不知道它存在）",
            spec.name
        );
    }
}

#[test]
fn unknown_command_is_rejected_by_name() {
    let err = crate::run(&["frobnicate".to_string()]).unwrap_err();
    assert_eq!(err.code, "BAD_ARGS");
    assert!(err.message.contains("未知子命令"), "{}", err.message);
}

#[test]
fn unknown_command_help_does_not_look_successful() {
    let err = crate::run(&["frobnicate".to_string(), "--help".to_string()]).unwrap_err();
    assert_eq!(err.code, "BAD_ARGS");
}

#[test]
fn admin_gate_passes_on_this_machine() {
    // 本机是管理员会话；如果这条失败，说明测试环境本身不是管理员，属于环境问题。
    match crate::win::integrity_rid(std::process::id()) {
        Some(rid) if rid < 0x3000 => {
            assert!(crate::ensure_admin().is_err(), "低完整性时应该拒绝");
        }
        _ => assert!(crate::ensure_admin().is_ok()),
    }
}

#[test]
fn integrity_levels_map_to_names() {
    assert_eq!(crate::win::integrity_name(0x0000), "untrusted");
    assert_eq!(crate::win::integrity_name(0x1000), "low");
    assert_eq!(crate::win::integrity_name(0x2000), "medium");
    assert_eq!(crate::win::integrity_name(0x2100), "medium");
    assert_eq!(crate::win::integrity_name(0x3000), "high");
    assert_eq!(crate::win::integrity_name(0x4000), "system");
    assert_eq!(crate::win::integrity_name(0x5000), "protected");
}

#[test]
fn our_own_integrity_is_readable() {
    let label = crate::win::integrity_label(std::process::id());
    assert!(
        label.starts_with("medium") || label.starts_with("high") || label.starts_with("system"),
        "got {label}"
    );
}

#[test]
fn snapshot_ids_are_unique_and_prefixed() {
    let a = crate::store::new_id();
    std::thread::sleep(std::time::Duration::from_millis(3));
    let b = crate::store::new_id();
    assert!(a.starts_with('s'));
    assert_ne!(a, b);
}

#[test]
fn rect_center_and_empty() {
    let r = crate::win::Rect {
        x: 100,
        y: 50,
        width: 40,
        height: 20,
    };
    assert_eq!(r.center(), (120, 60));
    assert!(!r.is_empty());
    assert!(
        crate::win::Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0
        }
        .is_empty()
    );
}

#[test]
fn window_id_roundtrip_is_stable_format() {
    assert_eq!(format!("w-{:x}", 0x1a2bu64), "w-1a2b");
}

#[test]
fn validate_accepts_double_dash_and_rejects_unknown() {
    let ok = Args::new(&argv(&["--", "click", "@s1:e3"]));
    assert!(ok.validate("elevate").is_ok(), "-- 之后应该都是位置参数");
    let missing = Args::new(&["--window".to_string()]);
    assert_eq!(missing.validate("snap").unwrap_err().code, "BAD_ARGS");
    let unknown = Args::new(&["--windw".to_string(), "w-1".to_string()]);
    assert_eq!(unknown.validate("snap").unwrap_err().code, "BAD_ARGS");
    let eq = Args::new(&["--depth=7".to_string()]);
    assert!(eq.validate("snap").is_ok());
}

#[test]
fn per_command_validation_rejects_foreign_flags() {
    let a = Args::new(&argv(&["--window", "w-1", "--seconds", "5"]));
    assert_eq!(a.validate("windows").unwrap_err().code, "BAD_ARGS");
    assert!(a.validate("guard").is_ok());
    let pretty = Args::new(&argv(&["--pretty"]));
    assert!(pretty.validate("status").is_ok());
}

#[test]
fn new_id_is_unique_without_sleep() {
    use std::collections::HashSet;
    let ids: HashSet<String> = (0..10_000).map(|_| crate::store::new_id()).collect();
    assert_eq!(ids.len(), 10_000);
}

#[test]
fn every_role_token_is_a_selector() {
    for t in 50000..=50040 {
        let token = crate::uia::role_name(t);
        assert!(
            crate::is_role(token),
            "role_name({t}) = {token} 不能用于选择器"
        );
    }
}

#[test]
fn removed_features_stay_removed() {
    assert_eq!(
        crate::act::parse_mode("physical").err().unwrap().code,
        "BAD_ARGS"
    );
    let err = crate::run(&["focus".to_string(), "w-1".to_string()]).unwrap_err();
    assert_eq!(err.code, "DISABLED");
}

#[test]
fn snapshot_ids_must_be_safe() {
    assert!(crate::store::is_valid_sid("s1a2b3c4d5"));
    assert!(!crate::store::is_valid_sid("s1a2b3c4d5e6f7g8h9i0j1k2l3"));
    assert!(!crate::store::is_valid_sid("../x"));
    assert!(!crate::store::is_valid_sid("s..\\x"));
    assert!(!crate::store::is_valid_sid("x123"));
}

#[test]
fn every_command_is_documented_in_the_skill() {
    let skill = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/SKILL.md"))
        .expect("读不到 SKILL.md");
    for spec in crate::commands::COMMANDS {
        if matches!(spec.name, "focus" | "scrollinto") {
            continue; // 已禁用 / 是 scrollitem 的别名
        }
        assert!(
            skill.contains(spec.name),
            "命令 {} 没写进 SKILL.md（agent 会不知道它存在）",
            spec.name
        );
    }
}

#[test]
fn ref_can_be_passed_as_option() {
    // --ref 与位置参数等价：info 应该走到"快照不存在"，而不是"缺少参数"。
    let err = crate::dispatch("info", &Args::new(&argv(&["--ref", "@snope:e1"]))).unwrap_err();
    assert_eq!(err.code, "SNAPSHOT_NOT_FOUND");
    let err = crate::dispatch("info", &Args::new(&argv(&[]))).unwrap_err();
    assert_eq!(err.code, "BAD_ARGS");
}

#[test]
fn readme_documents_commands_and_version() {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("读不到 README.md");
    assert!(readme.contains("0.2.0"), "README 没写当前版本");
    for spec in crate::commands::COMMANDS {
        if matches!(spec.name, "focus" | "scrollinto" | "help") {
            continue;
        }
        assert!(
            readme.contains(spec.name),
            "命令 {} 没写进 README",
            spec.name
        );
    }
}

#[test]
fn every_error_code_is_documented_in_the_skill() {
    let skill = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/SKILL.md"))
        .expect("读不到 SKILL.md");
    for code in crate::error::CODES {
        assert!(
            skill.contains(code),
            "错误码 {code} 没写进 SKILL.md 的错误表"
        );
    }
}

#[test]
fn watchdog_budget_follows_declared_timeout() {
    let with_timeout = crate::cli::Args::new(&argv(&["--timeout-ms", "5000"]));
    assert_eq!(
        crate::watchdog_budget(&with_timeout).as_millis(),
        5_000 + 30_000
    );
    let with_seconds = crate::cli::Args::new(&argv(&["--seconds", "10"]));
    assert_eq!(
        crate::watchdog_budget(&with_seconds).as_millis(),
        10_000 + 30_000
    );
    let plain = crate::cli::Args::new(&argv(&[]));
    assert!(crate::watchdog_budget(&plain).as_millis() >= 5_000);
}

#[test]
fn dispatch_has_a_branch_for_every_command() {
    // 往 COMMANDS 里加命令却忘了加 dispatch 分支时，这里会拿到"未知子命令"。
    for spec in crate::commands::COMMANDS {
        if matches!(spec.name, "help" | "guard" | "selftest" | "run") {
            continue; // 只读/会阻塞/读 stdin，留给集成测试
        }
        if let Err(e) = crate::dispatch(spec.name, &Args::new(&[])) {
            assert_ne!(
                e.message,
                format!("未知子命令：{}", spec.name),
                "命令 {} 在命令表里但没有 dispatch 分支",
                spec.name
            );
        }
    }
}

#[test]
fn walk_stats_combine_both_truncations() {
    assert!(!uia::WalkStats::default().truncated());
    assert!(
        uia::WalkStats {
            cut_by_depth: true,
            cut_by_limit: false
        }
        .truncated()
    );
    assert!(
        uia::WalkStats {
            cut_by_limit: true,
            cut_by_depth: false
        }
        .truncated()
    );
}

#[test]
fn result_file_must_stay_under_state_dir() {
    let bad =
        crate::allowed_result_file(std::path::Path::new("C:\\Windows\\Temp\\elevated-x.json"));
    assert!(bad.is_err());
}
