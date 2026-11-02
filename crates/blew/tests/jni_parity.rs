//! Verify that every Kotlin `external fun` declaration in `BleCentralManager.kt`
//! and `BlePeripheralManager.kt` has a matching `#[unsafe(no_mangle)] extern "C"`
//! function in `jni_hooks.rs`, and vice-versa.
//!
//! This catches JNI linkage mismatches at test time rather than at Android runtime.

use std::collections::BTreeSet;

const JNI_HOOKS_RS: &str = include_str!("../src/platform/android/jni_hooks.rs");
const CENTRAL_KT: &str =
    include_str!("../android/src/main/java/org/jakebot/blew/BleCentralManager.kt");
const PERIPHERAL_KT: &str =
    include_str!("../android/src/main/java/org/jakebot/blew/BlePeripheralManager.kt");
// `BlewPluginNative`'s Kotlin lives in this crate's Android module but its Rust
// hooks live in `tauri-plugin-blew`, so the pair falls between the two crates
// and nothing checked it. Reaching across from here keeps one copy of the
// parser rather than duplicating it in the other crate's tests.
const PLUGIN_KT: &str = include_str!("../android/src/main/java/org/jakebot/blew/BlewPlugin.kt");
const PLUGIN_RS: &str = include_str!("../../tauri-plugin-blew/src/lib.rs");

/// A JNI-crossing function reduced to what has to match on both sides.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
struct Signature {
    name: String,
    /// Parameter types, normalised to the Kotlin spelling.
    params: Vec<String>,
    /// Return type, normalised to the Kotlin spelling. `Unit` for no return.
    returns: String,
}

/// Collect the text between the parens of a declaration that may wrap lines.
fn param_block(lines: &[&str], start: usize) -> Option<String> {
    let mut depth = 0_i32;
    let mut out = String::new();
    for line in &lines[start..] {
        for ch in line.chars() {
            match ch {
                '(' => {
                    depth += 1;
                    if depth == 1 {
                        continue;
                    }
                }
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(out);
                    }
                }
                _ => {}
            }
            if depth >= 1 {
                out.push(ch);
            }
        }
        out.push(' ');
    }
    None
}

fn split_params(block: &str) -> Vec<&str> {
    block
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect()
}

/// Extract Kotlin `external fun` signatures from a `.kt` file.
fn kotlin_external_funs(source: &str) -> BTreeSet<Signature> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = BTreeSet::new();
    for (i, line) in lines.iter().enumerate() {
        // Not `strip_prefix`: the declaration is sometimes written
        // `@JvmStatic external fun foo()` on a single line.
        let Some(rest) = line.split_once("external fun ").map(|(_, rest)| rest) else {
            continue;
        };
        let Some(name) = rest.split('(').next() else {
            continue;
        };
        let Some(block) = param_block(&lines, i) else {
            continue;
        };
        let params = split_params(&block)
            .iter()
            // "deviceName: String?" -> "String". `splitn` rather than `split`
            // so a qualified Rust path like `jni::sys::jboolean` survives.
            .filter_map(|p| p.splitn(2, ':').nth(1))
            .map(|ty| ty.trim().trim_end_matches('?').to_string())
            .collect();
        out.insert(Signature {
            name: name.trim().to_string(),
            params,
            returns: kotlin_return_type(&lines, i),
        });
    }
    out
}

/// Read the `: Type` that follows a Kotlin declaration's closing paren.
/// Absent means `Unit`.
fn kotlin_return_type(lines: &[&str], start: usize) -> String {
    let mut depth = 0_i32;
    for line in &lines[start..] {
        for (idx, ch) in line.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let tail = line[idx + 1..].trim();
                        return match tail.strip_prefix(':') {
                            Some(ty) => ty.trim().trim_end_matches('?').to_string(),
                            None => "Unit".to_string(),
                        };
                    }
                }
                _ => {}
            }
        }
    }
    "Unit".to_string()
}

/// Read the `-> Type` that follows a Rust declaration's closing paren.
fn rust_return_type(lines: &[&str], start: usize) -> String {
    let mut depth = 0_i32;
    for line in &lines[start..] {
        for (idx, ch) in line.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let tail = line[idx + 1..].trim();
                        return match tail.strip_prefix("->") {
                            Some(ty) => rust_type_to_kotlin(ty.trim().trim_end_matches('{').trim()),
                            None => "Unit".to_string(),
                        };
                    }
                }
                _ => {}
            }
        }
    }
    "Unit".to_string()
}

/// Map a Rust JNI parameter type to the Kotlin type it must correspond to.
/// Unknown types map to themselves so a mismatch shows up rather than passing.
fn rust_type_to_kotlin(ty: &str) -> String {
    // `tauri-plugin-blew` writes these fully qualified (`jni::sys::jboolean`);
    // blew's hooks import them. Compare on the last path segment either way.
    let ty = ty.trim().rsplit("::").next().unwrap_or(ty).trim();
    match ty {
        "JString" => "String",
        "jint" => "Int",
        "jlong" => "Long",
        "jboolean" => "Boolean",
        "jfloat" => "Float",
        "jdouble" => "Double",
        "jshort" => "Short",
        "jbyte" => "Byte",
        "JByteArray" => "ByteArray",
        "JObjectArray" => "Array",
        "JContext" => "Context",
        "()" => "Unit",
        other => other,
    }
    .to_string()
}

/// Extract Rust JNI signatures from `jni_hooks.rs`, keyed by Kotlin class.
///
/// Pattern: `Java_org_jakebot_blew_{ClassName}_{methodName}`. The leading
/// `env` and `class` parameters are JNI boilerplate and are dropped, so what
/// remains lines up 1:1 with the Kotlin declaration.
fn rust_jni_symbols(source: &str) -> BTreeSet<(String, Signature)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = BTreeSet::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let Some(sym) = trimmed.strip_prefix("pub unsafe extern \"C\" fn ") else {
            continue;
        };
        let Some(sym) = sym.split('(').next() else {
            continue;
        };
        let Some(stem) = sym.strip_prefix("Java_org_jakebot_blew_") else {
            continue;
        };
        let Some(underscore) = stem.find('_') else {
            continue;
        };
        let class = &stem[..underscore];
        let method = &stem[underscore + 1..];
        let Some(block) = param_block(&lines, i) else {
            continue;
        };
        let params = split_params(&block)
            .iter()
            .skip(2) // env, class
            .filter_map(|p| p.splitn(2, ':').nth(1))
            .map(|ty| rust_type_to_kotlin(ty.trim().trim_start_matches("mut ")))
            .collect();
        out.insert((
            class.to_string(),
            Signature {
                name: method.to_string(),
                params,
                returns: rust_return_type(&lines, i),
            },
        ));
    }
    out
}

fn rust_symbols_for_class(all: &BTreeSet<(String, Signature)>, class: &str) -> BTreeSet<Signature> {
    all.iter()
        .filter(|(c, _)| c == class)
        .map(|(_, s)| s.clone())
        .collect()
}

/// Compare one Kotlin class against its Rust hooks, by name *and* signature.
fn assert_parity(class: &str, kt_source: &str, rs_source: &str) {
    let kt_funs = kotlin_external_funs(kt_source);
    let rust_funs = rust_symbols_for_class(&rust_jni_symbols(rs_source), class);

    let kt_names: BTreeSet<&String> = kt_funs.iter().map(|s| &s.name).collect();
    let rust_names: BTreeSet<&String> = rust_funs.iter().map(|s| &s.name).collect();

    let mut failures = Vec::new();

    let kt_only: Vec<_> = kt_names.difference(&rust_names).collect();
    if !kt_only.is_empty() {
        failures.push(format!(
            "Kotlin `external fun` with no Rust implementation: {kt_only:?}"
        ));
    }
    let rust_only: Vec<_> = rust_names.difference(&kt_names).collect();
    if !rust_only.is_empty() {
        failures.push(format!(
            "Rust JNI symbol with no Kotlin `external fun`: {rust_only:?}"
        ));
    }

    // Same name on both sides, different parameters. This is the failure the
    // JVM reports at call time as a crash, not at link time.
    for kt in &kt_funs {
        if let Some(rs) = rust_funs.iter().find(|r| r.name == kt.name)
            && (rs.params != kt.params || rs.returns != kt.returns)
        {
            failures.push(format!(
                "signature mismatch for `{}`:\n    Kotlin: {:?} -> {}\n    Rust:   {:?} -> {}",
                kt.name, kt.params, kt.returns, rs.params, rs.returns
            ));
        }
    }

    assert!(failures.is_empty(), "{class}:\n{}", failures.join("\n"));
}

#[test]
fn central_jni_parity() {
    assert_parity("BleCentralManager", CENTRAL_KT, JNI_HOOKS_RS);
}

#[test]
fn peripheral_jni_parity() {
    assert_parity("BlePeripheralManager", PERIPHERAL_KT, JNI_HOOKS_RS);
}

#[test]
fn plugin_jni_parity() {
    assert_parity("BlewPluginNative", PLUGIN_KT, PLUGIN_RS);
}
