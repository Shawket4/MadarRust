//! authz-gen: generate every permission artefact from `authz/spec/capabilities.toml`.
//!
//! ```text
//! cd MadarRust/authz/gen
//! cargo run                                                      # Rust only
//! cargo run -- --dashboard ../../../MadarDashboard --pos ../../../madar
//! cargo run -- --check [--dashboard ... --pos ...]               # CI: fail on drift
//! ```
//!
//! Outputs:
//! - `src/generated.rs` and `crate.hash` (this crate)
//! - `<dashboard>/src/generated/capabilities.ts`
//! - `<pos>/packages/app_core/lib/src/generated/capabilities.dart`
//! - `<pos>/rust-core/crates/madar-authz/` (a byte-identical copy of this crate)

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Deserialize)]
struct Spec {
    spec_version: u32,
    groups: BTreeMap<String, Label>,
    cap: Vec<CapSpec>,
    templates: BTreeMap<String, TemplateSpec>,
    roles: BTreeMap<String, Label>,
}

#[derive(Deserialize, Clone)]
struct Label {
    en: String,
    ar: String,
}

#[derive(Deserialize, Clone)]
struct CapSpec {
    id: u16,
    key: String,
    legacy: Option<String>,
    group: String,
    tier: String,
    risk: String,
    defaults: String,
    #[serde(default)]
    core: String,
    #[serde(default)]
    approval: bool,
    #[serde(default)]
    limits: Vec<String>,
    #[serde(default)]
    pos: bool,
    #[serde(default)]
    protected: bool,
    en: String,
    ar: String,
    hint_en: Option<String>,
    hint_ar: Option<String>,
}

#[derive(Deserialize)]
struct TemplateSpec {
    version: u32,
    en: String,
    ar: String,
    roles: Vec<String>,
    #[serde(default)]
    add: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    remove: BTreeMap<String, Vec<String>>,
    /// role -> capability -> default limits on a provisioned org's grant.
    #[serde(default)]
    limits: BTreeMap<String, BTreeMap<String, LimitSpec>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LimitSpec {
    max_amount: Option<i64>,
    max_percent: Option<i64>,
    max_value: Option<i64>,
    max_age_minutes: Option<i64>,
    #[serde(default)]
    own: bool,
}

impl LimitSpec {
    fn keys(&self) -> Vec<&'static str> {
        let mut v = vec![];
        if self.max_amount.is_some() {
            v.push("max_amount");
        }
        if self.max_percent.is_some() {
            v.push("max_percent");
        }
        if self.max_value.is_some() {
            v.push("max_value");
        }
        if self.max_age_minutes.is_some() {
            v.push("max_age_minutes");
        }
        if self.own {
            v.push("own");
        }
        v
    }

    fn rust(&self) -> String {
        let o = |x: Option<i64>| match x {
            Some(n) => format!("Some({n})"),
            None => "None".into(),
        };
        format!(
            "Limits {{ max_amount: {}, max_percent: {}, max_value: {}, max_age_minutes: {}, own: {} }}",
            o(self.max_amount),
            o(self.max_percent),
            o(self.max_value),
            o(self.max_age_minutes),
            self.own
        )
    }
}

const WORDS: usize = 4;
const KIND_LETTERS: [(char, &str, &str); 5] = [
    ('o', "org_admin", "OrgAdmin"),
    ('m', "branch_manager", "BranchManager"),
    ('t', "teller", "Teller"),
    ('w', "waiter", "Waiter"),
    ('k', "kitchen", "Kitchen"),
];

fn fnv(parts: &[&[u8]]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for b in *p {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn variant(key: &str) -> String {
    key.split(['.', '_'])
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn camel(key: &str) -> String {
    let v = variant(key);
    let mut c = v.chars();
    match c.next() {
        Some(f) => f.to_ascii_lowercase().to_string() + c.as_str(),
        None => v,
    }
}

fn kinds_mask(s: &str) -> Result<u8, String> {
    let mut m = 0u8;
    for ch in s.chars() {
        let i = KIND_LETTERS
            .iter()
            .position(|(l, _, _)| *l == ch)
            .ok_or_else(|| format!("unknown role letter {ch:?}"))?;
        m |= 1 << i;
    }
    Ok(m)
}

fn kinds_list(s: &str) -> Vec<&'static str> {
    KIND_LETTERS
        .iter()
        .filter(|(l, _, _)| s.contains(*l))
        .map(|(_, k, _)| *k)
        .collect()
}

fn kind_variant(k: &str) -> Result<&'static str, String> {
    KIND_LETTERS
        .iter()
        .find(|(_, name, _)| *name == k)
        .map(|(_, _, v)| *v)
        .ok_or_else(|| format!("unknown role {k}"))
}

fn rs_str(s: &str) -> String {
    format!("{s:?}")
}

fn validate(spec: &Spec) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut legacy = BTreeSet::new();
    for c in &spec.cap {
        if !ids.insert(c.id) {
            return Err(format!("duplicate id {}", c.id));
        }
        if c.id == 0 || c.id as usize >= WORDS * 64 {
            return Err(format!("id {} out of range", c.id));
        }
        if !keys.insert(c.key.clone()) {
            return Err(format!("duplicate key {}", c.key));
        }
        if !c
            .key
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch == '.' || ch == '_')
        {
            return Err(format!("bad key {}", c.key));
        }
        if let Some(l) = &c.legacy {
            if !legacy.insert(l.clone()) || !l.contains(':') {
                return Err(format!("legacy cell {l} duplicated or malformed"));
            }
        }
        if !spec.groups.contains_key(&c.group) {
            return Err(format!("{}: unknown group {}", c.key, c.group));
        }
        if !["core", "configurable", "advanced", "legacy"].contains(&c.tier.as_str()) {
            return Err(format!("{}: bad tier", c.key));
        }
        if !["normal", "money", "pii", "admin"].contains(&c.risk.as_str()) {
            return Err(format!("{}: bad risk", c.key));
        }
        let d = kinds_mask(&c.defaults)?;
        let k = kinds_mask(&c.core)?;
        if k & !d != 0 {
            return Err(format!(
                "{}: core roles must be a subset of defaults",
                c.key
            ));
        }
        if (c.tier == "core") != (k != 0) {
            return Err(format!("{}: tier core iff core roles are listed", c.key));
        }
        if c.risk == "admin" && d & 0b11100 != 0 {
            return Err(format!(
                "{}: admin capability granted to a floor role",
                c.key
            ));
        }
        for l in &c.limits {
            if !["max_amount", "max_percent", "max_value", "max_age_minutes", "own"]
                .contains(&l.as_str())
            {
                return Err(format!("{}: bad limit {l}", c.key));
            }
        }
        if c.en.trim().is_empty() || c.ar.trim().is_empty() || c.en == c.ar {
            return Err(format!("{}: EN and AR labels required and distinct", c.key));
        }
    }
    for (name, t) in &spec.templates {
        for r in &t.roles {
            kind_variant(r)?;
        }
        for (r, caps) in t.add.iter().chain(t.remove.iter()) {
            kind_variant(r)?;
            for c in caps {
                if !keys.contains(c) {
                    return Err(format!("template {name}: unknown capability {c}"));
                }
            }
        }
        for (r, caps) in &t.limits {
            kind_variant(r)?;
            for (key, l) in caps {
                let Some(c) = spec.cap.iter().find(|x| &x.key == key) else {
                    return Err(format!("template {name}: unknown capability {key}"));
                };
                if l.keys().is_empty() {
                    return Err(format!("template {name}: empty limits on {key}"));
                }
                for k in l.keys() {
                    if !c.limits.iter().any(|x| x == k) {
                        return Err(format!(
                            "template {name}: {key} does not declare limit {k}"
                        ));
                    }
                }
            }
        }
        for (r, caps) in &t.add {
            for key in caps {
                let c = spec.cap.iter().find(|x| &x.key == key).unwrap();
                if c.risk == "admin" && ["teller", "waiter", "kitchen"].contains(&r.as_str()) {
                    return Err(format!("template {name}: admin {key} on {r}"));
                }
            }
        }
    }
    Ok(())
}

fn gen_rust(spec: &Spec, spec_hash: &str) -> Result<String, String> {
    let mut caps = spec.cap.clone();
    caps.sort_by_key(|c| c.id);
    let mut o = String::new();
    writeln!(
        o,
        "// @generated by authz-gen from authz/spec/capabilities.toml. DO NOT EDIT."
    )
    .unwrap();
    writeln!(o, "#![allow(clippy::all)]\n").unwrap();
    writeln!(
        o,
        "use crate::{{CapMeta, GroupMeta, Kinds, LimitKey, Limits, Risk, RoleKind, TemplateMeta, Tier}};\n"
    )
    .unwrap();
    writeln!(o, "pub const SPEC_VERSION: u32 = {};", spec.spec_version).unwrap();
    writeln!(o, "pub const SPEC_HASH: &str = {};", rs_str(spec_hash)).unwrap();
    writeln!(o, "pub const WORDS: usize = {WORDS};\n").unwrap();
    writeln!(
        o,
        "#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]"
    )
    .unwrap();
    writeln!(o, "#[repr(u16)]\npub enum Cap {{").unwrap();
    for c in &caps {
        writeln!(o, "    {} = {},", variant(&c.key), c.id).unwrap();
    }
    writeln!(o, "}}\n").unwrap();

    let tier = |t: &str| match t {
        "core" => "Tier::Core",
        "configurable" => "Tier::Configurable",
        "advanced" => "Tier::Advanced",
        _ => "Tier::Legacy",
    };
    let risk = |r: &str| match r {
        "money" => "Risk::Money",
        "pii" => "Risk::Pii",
        "admin" => "Risk::Admin",
        _ => "Risk::Normal",
    };
    let limit = |l: &str| match l {
        "max_amount" => "LimitKey::MaxAmount",
        "max_percent" => "LimitKey::MaxPercent",
        "max_value" => "LimitKey::MaxValue",
        "max_age_minutes" => "LimitKey::MaxAgeMinutes",
        _ => "LimitKey::Own",
    };
    writeln!(o, "pub static CAPS: &[CapMeta] = &[").unwrap();
    for c in &caps {
        let legacy = match &c.legacy {
            Some(l) => {
                let (r, a) = l.split_once(':').unwrap();
                format!("Some(({}, {}))", rs_str(r), rs_str(a))
            }
            None => "None".into(),
        };
        let opt = |s: &Option<String>| {
            s.as_ref()
                .map_or("None".to_string(), |v| format!("Some({})", rs_str(v)))
        };
        writeln!(
            o,
            "    CapMeta {{ cap: Cap::{}, key: {}, legacy: {}, group: {}, tier: {}, risk: {}, defaults: Kinds({}), core: Kinds({}), approval: {}, limits: &[{}], pos: {}, protected: {}, en: {}, ar: {}, hint_en: {}, hint_ar: {} }},",
            variant(&c.key),
            rs_str(&c.key),
            legacy,
            rs_str(&c.group),
            tier(&c.tier),
            risk(&c.risk),
            kinds_mask(&c.defaults)?,
            kinds_mask(&c.core)?,
            c.approval,
            c.limits.iter().map(|l| limit(l)).collect::<Vec<_>>().join(", "),
            c.pos,
            c.protected,
            rs_str(&c.en),
            rs_str(&c.ar),
            opt(&c.hint_en),
            opt(&c.hint_ar),
        )
        .unwrap();
    }
    writeln!(o, "];\n").unwrap();

    writeln!(o, "pub static GROUPS: &[GroupMeta] = &[").unwrap();
    for (k, l) in &spec.groups {
        writeln!(
            o,
            "    GroupMeta {{ key: {}, en: {}, ar: {} }},",
            rs_str(k),
            rs_str(&l.en),
            rs_str(&l.ar)
        )
        .unwrap();
    }
    writeln!(o, "];\n").unwrap();

    writeln!(o, "pub static ROLE_LABELS: &[(RoleKind, &str, &str)] = &[").unwrap();
    for (k, l) in &spec.roles {
        writeln!(
            o,
            "    (RoleKind::{}, {}, {}),",
            kind_variant(k)?,
            rs_str(&l.en),
            rs_str(&l.ar)
        )
        .unwrap();
    }
    writeln!(o, "];\n").unwrap();

    writeln!(o, "pub static TEMPLATES: &[TemplateMeta] = &[").unwrap();
    for (k, t) in &spec.templates {
        let roles: Result<Vec<_>, _> = t
            .roles
            .iter()
            .map(|r| kind_variant(r).map(|v| format!("RoleKind::{v}")))
            .collect();
        let pairs = |m: &BTreeMap<String, Vec<String>>| -> Result<String, String> {
            let mut v = vec![];
            for (r, caps) in m {
                for c in caps {
                    v.push(format!(
                        "(RoleKind::{}, Cap::{})",
                        kind_variant(r)?,
                        variant(c)
                    ));
                }
            }
            Ok(v.join(", "))
        };
        let mut limits = vec![];
        for (r, caps) in &t.limits {
            for (c, l) in caps {
                limits.push(format!(
                    "(RoleKind::{}, Cap::{}, {})",
                    kind_variant(r)?,
                    variant(c),
                    l.rust()
                ));
            }
        }
        writeln!(
            o,
            "    TemplateMeta {{ key: {}, version: {}, en: {}, ar: {}, roles: &[{}], add: &[{}], remove: &[{}], limits: &[{}] }},",
            rs_str(k),
            t.version,
            rs_str(&t.en),
            rs_str(&t.ar),
            roles?.join(", "),
            pairs(&t.add)?,
            pairs(&t.remove)?,
            limits.join(", ")
        )
        .unwrap();
    }
    writeln!(o, "];").unwrap();
    Ok(o)
}

fn js(s: &str) -> String {
    serde_json_string(s)
}

fn serde_json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn gen_ts(spec: &Spec, spec_hash: &str) -> String {
    let mut caps = spec.cap.clone();
    caps.sort_by_key(|c| c.id);
    let mut o = String::new();
    writeln!(
        o,
        "// @generated by authz-gen from MadarRust/authz/spec/capabilities.toml. DO NOT EDIT."
    )
    .unwrap();
    writeln!(o, "/* eslint-disable */\n").unwrap();
    writeln!(o, "export const SPEC_VERSION = {};", spec.spec_version).unwrap();
    writeln!(o, "export const SPEC_HASH = {};\n", js(spec_hash)).unwrap();
    writeln!(
        o,
        "export type RoleKind = 'org_admin' | 'branch_manager' | 'teller' | 'waiter' | 'kitchen';"
    )
    .unwrap();
    writeln!(
        o,
        "export type CapabilityTier = 'core' | 'configurable' | 'advanced' | 'legacy';"
    )
    .unwrap();
    writeln!(
        o,
        "export type CapabilityRisk = 'normal' | 'money' | 'pii' | 'admin';"
    )
    .unwrap();
    writeln!(
        o,
        "export type LimitKey =\n  | 'max_amount'\n  | 'max_percent'\n  | 'max_value'\n  | 'max_age_minutes'\n  | 'own';\n"
    )
    .unwrap();
    writeln!(o, "export type Capability =").unwrap();
    for c in &caps {
        writeln!(o, "  | {}", js(&c.key)).unwrap();
    }
    writeln!(o, ";\n").unwrap();
    writeln!(
        o,
        "/** Every capability key, for `Cap.X` style references. */"
    )
    .unwrap();
    writeln!(o, "export const Cap = {{").unwrap();
    for c in &caps {
        writeln!(o, "  {}: {} as Capability,", camel(&c.key), js(&c.key)).unwrap();
    }
    writeln!(o, "}} as const;\n").unwrap();
    writeln!(o, "export interface CapabilityMeta {{\n  id: number;\n  key: Capability;\n  legacy: string | null;\n  group: string;\n  tier: CapabilityTier;\n  risk: CapabilityRisk;\n  defaults: RoleKind[];\n  core: RoleKind[];\n  approval: boolean;\n  limits: LimitKey[];\n  pos: boolean;\n  protected: boolean;\n  en: string;\n  ar: string;\n  hintEn: string | null;\n  hintAr: string | null;\n}}\n").unwrap();
    writeln!(
        o,
        "export const CAPABILITIES: readonly CapabilityMeta[] = ["
    )
    .unwrap();
    let arr = |v: Vec<&str>| {
        format!(
            "[{}]",
            v.iter().map(|x| js(x)).collect::<Vec<_>>().join(", ")
        )
    };
    let opt = |s: &Option<String>| s.as_ref().map_or("null".to_string(), |v| js(v));
    for c in &caps {
        writeln!(
            o,
            "  {{ id: {}, key: {}, legacy: {}, group: {}, tier: {}, risk: {}, defaults: {}, core: {}, approval: {}, limits: {}, pos: {}, protected: {}, en: {}, ar: {}, hintEn: {}, hintAr: {} }},",
            c.id,
            js(&c.key),
            c.legacy.as_ref().map_or("null".into(), |l| js(l)),
            js(&c.group),
            js(&c.tier),
            js(&c.risk),
            arr(kinds_list(&c.defaults)),
            arr(kinds_list(&c.core)),
            c.approval,
            arr(c.limits.iter().map(String::as_str).collect()),
            c.pos,
            c.protected,
            js(&c.en),
            js(&c.ar),
            opt(&c.hint_en),
            opt(&c.hint_ar),
        )
        .unwrap();
    }
    writeln!(o, "];\n").unwrap();
    writeln!(
        o,
        "export const CAPABILITY_GROUPS: readonly {{ key: string; en: string; ar: string }}[] = ["
    )
    .unwrap();
    for (k, l) in &spec.groups {
        writeln!(
            o,
            "  {{ key: {}, en: {}, ar: {} }},",
            js(k),
            js(&l.en),
            js(&l.ar)
        )
        .unwrap();
    }
    writeln!(o, "];\n").unwrap();
    writeln!(
        o,
        "export const ROLE_KIND_LABELS: Record<RoleKind, {{ en: string; ar: string }}> = {{"
    )
    .unwrap();
    for (k, l) in &spec.roles {
        writeln!(o, "  {k}: {{ en: {}, ar: {} }},", js(&l.en), js(&l.ar)).unwrap();
    }
    writeln!(o, "}};").unwrap();
    o
}

fn gen_dart(spec: &Spec, spec_hash: &str) -> String {
    let mut caps = spec.cap.clone();
    caps.sort_by_key(|c| c.id);
    let mut o = String::new();
    writeln!(
        o,
        "// @generated by authz-gen from MadarRust/authz/spec/capabilities.toml. DO NOT EDIT."
    )
    .unwrap();
    writeln!(o, "// ignore_for_file: constant_identifier_names\n").unwrap();
    writeln!(
        o,
        "/// Capability keys the till gates on. Ask the core (`bridge.can`), never a role."
    )
    .unwrap();
    writeln!(o, "abstract final class Cap {{").unwrap();
    writeln!(o, "  static const int specVersion = {};", spec.spec_version).unwrap();
    writeln!(o, "  static const String specHash = '{spec_hash}';").unwrap();
    for c in caps.iter().filter(|c| c.pos || c.tier != "legacy") {
        let line = format!("  static const String {} = '{}';", camel(&c.key), c.key);
        if line.len() > 80 {
            writeln!(
                o,
                "  static const String {} =\n      '{}';",
                camel(&c.key),
                c.key
            )
            .unwrap();
        } else {
            writeln!(o, "{line}").unwrap();
        }
    }
    writeln!(o, "}}").unwrap();
    o
}

/// Files of the crate that the crate hash covers, relative to the crate root.
const CRATE_FILES: &[&str] = &[
    "Cargo.toml",
    "src/lib.rs",
    "src/generated.rs",
    "src/guard.rs",
    "src/snapshot.rs",
    "src/tests.rs",
];

fn crate_hash(root: &Path) -> Result<String, String> {
    let mut bufs = vec![];
    for f in CRATE_FILES {
        bufs.push(std::fs::read(root.join(f)).map_err(|e| format!("{f}: {e}"))?);
    }
    let parts: Vec<&[u8]> = bufs.iter().map(Vec::as_slice).collect();
    Ok(fnv(&parts))
}

struct Out {
    check: bool,
    stale: Vec<PathBuf>,
}

impl Out {
    fn put(&mut self, path: &Path, content: &[u8]) -> Result<(), String> {
        let current = std::fs::read(path).ok();
        if current.as_deref() == Some(content) {
            return Ok(());
        }
        if self.check {
            self.stale.push(path.to_path_buf());
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, content).map_err(|e| format!("{}: {e}", path.display()))?;
        println!("wrote {}", path.display());
        Ok(())
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("authz-gen: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crate");
    let mut args = std::env::args().skip(1);
    let mut spec_path = crate_root.join("../spec/capabilities.toml");
    let mut dashboard: Option<PathBuf> = None;
    let mut pos: Option<PathBuf> = None;
    let mut check = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--check" => check = true,
            "--spec" => spec_path = args.next().ok_or("--spec needs a path")?.into(),
            "--dashboard" => {
                dashboard = Some(args.next().ok_or("--dashboard needs a path")?.into())
            }
            "--pos" => pos = Some(args.next().ok_or("--pos needs a path")?.into()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let raw =
        std::fs::read_to_string(&spec_path).map_err(|e| format!("{}: {e}", spec_path.display()))?;
    let spec: Spec = toml::from_str(&raw).map_err(|e| e.to_string())?;
    validate(&spec)?;
    let spec_hash = fnv(&[raw.as_bytes()]);

    let mut out = Out {
        check,
        stale: vec![],
    };
    out.put(
        &crate_root.join("src/generated.rs"),
        gen_rust(&spec, &spec_hash)?.as_bytes(),
    )?;
    // The crate hash is over the files as they now stand (generated.rs included).
    let hash = crate_hash(&crate_root)?;
    out.put(
        &crate_root.join("crate.hash"),
        format!("{hash}\n").as_bytes(),
    )?;

    if let Some(d) = dashboard {
        out.put(
            &d.join("src/generated/capabilities.ts"),
            gen_ts(&spec, &spec_hash).as_bytes(),
        )?;
    }
    if let Some(p) = pos {
        out.put(
            &p.join("packages/app_core/lib/src/generated/capabilities.dart"),
            gen_dart(&spec, &spec_hash).as_bytes(),
        )?;
        let vendored = p.join("rust-core/crates/madar-authz");
        for f in CRATE_FILES.iter().chain(["crate.hash"].iter()) {
            let bytes = std::fs::read(crate_root.join(f)).map_err(|e| format!("{f}: {e}"))?;
            out.put(&vendored.join(f), &bytes)?;
        }
    }

    if !out.stale.is_empty() {
        for p in &out.stale {
            eprintln!("stale: {}", p.display());
        }
        return Err("generated files are stale; run authz-gen".into());
    }
    Ok(())
}
