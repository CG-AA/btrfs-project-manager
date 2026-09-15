//! `bpm config`: loaded global config or a project's merged config with origins.

use crate::cli::ConfigArgs;
use crate::ctx::Ctx;
use crate::output::emit;
use anyhow::Result;

fn render_table(
    t: &toml::Table,
    prefix: &str,
    origins: &std::collections::BTreeMap<String, String>,
    show: bool,
    out: &mut String,
) {
    let mut subs = Vec::new();
    for (k, v) in t {
        if let toml::Value::Table(sub) = v {
            subs.push((k, sub));
            continue;
        }
        let key = format!("{prefix}{k}");
        let origin =
            if show { origins.get(&key).map(|o| format!("  # {o}")).unwrap_or_default() } else { String::new() };
        out.push_str(&format!("{k} = {v}{origin}\n"));
    }
    for (k, sub) in subs {
        out.push_str(&format!("\n[{prefix}{k}]\n"));
        render_table(sub, &format!("{prefix}{k}."), origins, show, out);
    }
}

pub fn run(ctx: &Ctx, a: ConfigArgs) -> Result<()> {
    if a.default {
        print!("{}", crate::config::DEFAULT_CONFIG_TOML);
        return Ok(());
    }
    match &a.project {
        None => {
            emit(ctx, &ctx.cfg, || {
                let src = ctx
                    .cfg_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "embedded default (no config file)".into());
                format!("# source: {src}\n{}", toml::to_string_pretty(&ctx.cfg).unwrap_or_default())
            });
        }
        Some(spec) => {
            let (root, name, path) = match crate::project::resolve(ctx, spec) {
                Ok(p) => (p.root.clone(), p.name().to_string(), p.path().to_path_buf()),
                Err(_) => {
                    let candidate = if spec.contains('/') {
                        std::path::PathBuf::from(spec)
                    } else {
                        ctx.roots().first().map(|r| r.path.join(spec)).unwrap_or_else(|| spec.into())
                    };
                    let path = std::path::absolute(candidate)?;
                    let (root, name) = crate::project::locate_path(ctx, &path)
                        .ok_or_else(|| crate::error::usage(format!("{spec} is not inside a configured root")))?;
                    let p = root.path.join(&name);
                    (root, name, p)
                }
            };
            let eff = crate::project::effective(ctx, &root, &name, &path)?;
            emit(ctx, &eff, || {
                let mut out = format!(
                    "# {} ({})\nmanaged = {}\nprofiles = {:?}\nbanlist = {:?}\nprimary_banlist = {:?}\nnever_precreate = {:?}\nsentinels = {:?}\n",
                    name,
                    path.display(),
                    eff.managed,
                    eff.profiles,
                    eff.banlist,
                    eff.primary_banlist,
                    eff.never_precreate,
                    eff.sentinels
                );
                if let Ok(toml::Value::Table(t)) = toml::Value::try_from(&eff.policy) {
                    out.push_str("\n# policy\n");
                    render_table(&t, "", &eff.origins, a.show_origin, &mut out);
                }
                out
            });
        }
    }
    Ok(())
}
