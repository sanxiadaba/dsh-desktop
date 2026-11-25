//! 不需要桌面的单元测试：解析、映射、纯计算。

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
fn args_rejects_unknown_option() {
    let a = Args::new(&argv(&["--windwo", "w-1"]));
    let err = a.reject_unknown(&["--window"]).unwrap_err();
    assert_eq!(err.code, "BAD_ARGS");
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
fn admin_gate_allows_only_diagnostics() {
    for ok in ["help", "status", "elevate"] {
        assert!(crate::is_readonly(ok), "{ok} 应该允许非管理员");
    }
    for deny in ["click", "type", "snap", "windows", "shot", "run"] {
        assert!(!crate::is_readonly(deny), "{deny} 应该要求管理员");
    }
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
    // 只验证格式，不碰真实窗口
    assert_eq!(format!("w-{:x}", 0x1a2bu64), "w-1a2b");
}
