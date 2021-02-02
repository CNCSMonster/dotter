use anyhow::{Context, Result};

use config::Cache;
use filesystem::load_file;
use handlebars_helpers::create_new_handlebars;

use std::io::{self, Read};
use std::path::Path;

use crate::args::Options;
use crate::config;
use crate::display_error;
use crate::file_state::{file_state_from_configuration, FileState};
use crate::filesystem;
use crate::handlebars_helpers;
use crate::hooks;
use crate::{actions::Action, filesystem::Filesystem};

/// Returns true if an error was printed
pub fn deploy(opt: &Options) -> Result<bool> {
    let mut patch = None;
    if opt.patch {
        debug!("Reading manual patch from stdin...");
        let mut patch_str = String::new();
        io::stdin()
            .read_to_string(&mut patch_str)
            .context("read patch from stdin")?;
        patch = Some(toml::from_str(&patch_str).context("parse patch into package")?);
    }
    trace!("Manual patch: {:#?}", patch);

    let mut config = config::load_configuration(&opt.local_config, &opt.global_config, patch)
        .context("get a configuration")?;

    let mut cache = if let Some(cache) = load_file(&opt.cache_file)? {
        cache
    } else {
        warn!("Cache file not found. Assuming cache is empty.");
        config::Cache::default()
    };

    let state = file_state_from_configuration(&config, &cache, &opt.cache_directory)
        .context("get file state")?;
    trace!("File state: {:#?}", state);

    let handlebars = create_new_handlebars(&mut config);

    debug!("Running pre-deploy hook");
    if opt.act {
        hooks::run_hook(
            &opt.pre_deploy,
            &opt.cache_directory,
            &handlebars,
            &config.variables,
        )
        .context("run pre-deploy hook")?;
    }

    let mut suggest_force = false;
    let mut error_occurred = false;

    let plan = plan_deploy(state);
    let (mut real_fs, mut dry_run_fs);
    let fs: &mut dyn Filesystem = if opt.act {
        real_fs = crate::filesystem::RealFilesystem::new(opt.interactive);
        &mut real_fs
    } else {
        dry_run_fs = crate::filesystem::DryRunFilesystem::new();
        &mut dry_run_fs
    };

    for action in plan {
        match action.run(fs, opt, &handlebars, &config.variables) {
            Ok(true) => action.affect_cache(&mut cache),
            Ok(false) => {
                suggest_force = true;
            }
            Err(e) => {
                error_occurred = true;
                display_error(e);
            }
        }
    }

    trace!("Actual symlinks: {:#?}", cache.symlinks);
    trace!("Actual templates: {:#?}", cache.templates);

    if suggest_force {
        error!("Some files were skipped. To ignore errors and overwrite unexpected target files, use the --force flag.");
        error_occurred = true;
    }

    if opt.act {
        filesystem::save_file(&opt.cache_file, cache).context("save cache")?;
    }

    debug!("Running post-deploy hook");
    if opt.act {
        hooks::run_hook(
            &opt.post_deploy,
            &opt.cache_directory,
            &handlebars,
            &config.variables,
        )
        .context("run post-deploy hook")?;
    }

    Ok(error_occurred)
}

pub fn undeploy(opt: Options) -> Result<bool> {
    let mut config = config::load_configuration(&opt.local_config, &opt.global_config, None)
        .context("get a configuration")?;

    let mut cache: config::Cache = filesystem::load_file(&opt.cache_file)?
        .context("load cache: Cannot undeploy without a cache.")?;

    let handlebars = create_new_handlebars(&mut config);

    debug!("Running pre-undeploy hook");
    if opt.act {
        hooks::run_hook(
            &opt.pre_undeploy,
            &opt.cache_directory,
            &handlebars,
            &config.variables,
        )
        .context("run pre-undeploy hook")?;
    }

    let mut suggest_force = false;
    let mut error_occurred = false;

    let plan = plan_undeploy(&cache, &opt.cache_directory);
    let (mut real_fs, mut dry_run_fs);
    let fs: &mut dyn Filesystem = if opt.act {
        real_fs = crate::filesystem::RealFilesystem::new(opt.interactive);
        &mut real_fs
    } else {
        dry_run_fs = crate::filesystem::DryRunFilesystem::new();
        &mut dry_run_fs
    };

    for action in plan {
        match action.run(fs, &opt, &handlebars, &config.variables) {
            Ok(true) => action.affect_cache(&mut cache),
            Ok(false) => {
                suggest_force = true;
            }
            Err(e) => {
                error_occurred = true;
                display_error(e);
            }
        }
    }

    if suggest_force {
        error!("Some files were skipped. To ignore errors and overwrite unexpected target files, use the --force flag.");
        error_occurred = true;
    }

    if opt.act {
        // Should be empty if everything went well, but if some things were skipped this contains
        // them.
        filesystem::save_file(&opt.cache_file, cache).context("save cache")?;
    }

    debug!("Running post-undeploy hook");
    if opt.act {
        hooks::run_hook(
            &opt.post_undeploy,
            &opt.cache_directory,
            &handlebars,
            &config.variables,
        )
        .context("run post-undeploy hook")?;
    }

    Ok(error_occurred)
}

fn plan_deploy(state: FileState) -> Vec<Action> {
    let mut actions = Vec::new();

    let FileState {
        desired_symlinks,
        desired_templates,
        existing_symlinks,
        existing_templates,
    } = state;

    for deleted_symlink in existing_symlinks.difference(&desired_symlinks).cloned() {
        actions.push(Action::DeleteSymlink {
            source: deleted_symlink.source,
            target: deleted_symlink.target.target,
        });
    }

    for deleted_template in existing_templates.difference(&desired_templates).cloned() {
        actions.push(Action::DeleteTemplate {
            source: deleted_template.source,
            cache: deleted_template.cache,
            target: deleted_template.target.target,
        });
    }

    for created_symlink in desired_symlinks.difference(&existing_symlinks) {
        actions.push(Action::CreateSymlink(created_symlink.clone()));
    }

    for created_template in desired_templates.difference(&existing_templates) {
        actions.push(Action::CreateTemplate(created_template.clone()));
    }

    for updated_symlink in desired_symlinks.intersection(&existing_symlinks) {
        actions.push(Action::UpdateSymlink(updated_symlink.clone()));
    }

    for updated_template in desired_templates.intersection(&existing_templates) {
        actions.push(Action::UpdateTemplate(updated_template.clone()));
    }

    actions
}

fn plan_undeploy(cache: &Cache, cache_directory: &Path) -> Vec<Action> {
    let mut actions = Vec::new();

    for (source, target) in &cache.symlinks {
        actions.push(Action::DeleteSymlink {
            source: source.clone(),
            target: target.clone(),
        });
    }

    for (source, target) in &cache.templates {
        let cache = cache_directory.join(&source);
        actions.push(Action::DeleteTemplate {
            source: source.clone(),
            cache: cache.clone(),
            target: target.clone(),
        });
    }

    actions
}

#[cfg(test)]
mod test {
    use crate::{
        config::{SymbolicTarget, TemplateTarget},
        filesystem::SymlinkComparison,
    };
    use crate::{
        file_state::{SymlinkDescription, TemplateDescription},
        filesystem::TemplateComparison,
    };

    use std::{
        collections::BTreeSet,
        path::{Path, PathBuf},
    };

    use super::*;

    use mockall::predicate::*;

    #[test]
    fn initial_deploy() {
        // File state
        let a = SymlinkDescription {
            source: "a_in".into(),
            target: SymbolicTarget {
                target: "a_out".into(),
                owner: None,
            },
        };
        let b = TemplateDescription {
            source: "b_in".into(),
            target: TemplateTarget {
                target: "b_out".into(),
                owner: None,
                append: None,
                prepend: None,
            },
            cache: "cache/b_cache".into(),
        };
        let file_state = FileState {
            desired_symlinks: maplit::btreeset! {
                a.clone()
            },
            desired_templates: maplit::btreeset! {
                b.clone()
            },
            existing_symlinks: BTreeSet::new(),
            existing_templates: BTreeSet::new(),
        };

        // Plan
        let actions = plan_deploy(file_state);
        assert_eq!(
            actions,
            [Action::CreateSymlink(a), Action::CreateTemplate(b)]
        );

        // Setup
        let mut fs = crate::filesystem::MockFilesystem::new();
        let mut seq = mockall::Sequence::new();

        let options = Options::default();
        let handlebars = handlebars::Handlebars::new();
        let variables = Default::default();

        fn path_eq(expected: &str) -> impl Fn(&Path) -> bool {
            let expected = PathBuf::from(expected);
            move |actual| actual == expected
        }

        // Action 1
        fs.expect_compare_symlink()
            .times(1)
            .with(function(path_eq("a_in")), function(path_eq("a_out")))
            .in_sequence(&mut seq)
            .returning(|_, _| Ok(SymlinkComparison::OnlySourceExists));
        fs.expect_create_dir_all()
            .times(1)
            .with(function(path_eq("")), eq(None)) // parent of a_out
            .in_sequence(&mut seq)
            .returning(|_, _| Ok(()));
        fs.expect_make_symlink()
            .times(1)
            .with(
                function(path_eq("a_out")),
                function(path_eq("a_in")),
                eq(None),
            )
            .in_sequence(&mut seq)
            .returning(|_, _, _| Ok(()));

        actions[0]
            .run(&mut fs, &options, &handlebars, &variables)
            .unwrap();

        fs.checkpoint();

        // Action 2
        fs.expect_compare_template()
            .times(1)
            .with(
                function(path_eq("b_out")),
                function(path_eq("cache/b_cache")),
            )
            .in_sequence(&mut seq)
            .returning(|_, _| Ok(TemplateComparison::BothMissing));
        fs.expect_create_dir_all()
            .times(1)
            .with(function(path_eq("")), eq(None)) // parent of b_out
            .in_sequence(&mut seq)
            .returning(|_, _| Ok(()));
        fs.expect_read_to_string()
            .times(1)
            .with(function(path_eq("b_in")))
            .in_sequence(&mut seq)
            .returning(|_| Ok("".into()));
        fs.expect_create_dir_all()
            .times(1)
            .with(function(path_eq("cache")), eq(None))
            .in_sequence(&mut seq)
            .returning(|_, _| Ok(()));
        fs.expect_write()
            .times(1)
            .with(function(path_eq("cache/b_cache")), eq(String::from("")))
            .in_sequence(&mut seq)
            .returning(|_, _| Ok(()));
        fs.expect_copy_file()
            .times(1)
            .with(
                function(path_eq("cache/b_cache")),
                function(path_eq("b_out")),
                eq(None),
            )
            .in_sequence(&mut seq)
            .returning(|_, _, _| Ok(()));
        fs.expect_copy_permissions()
            .times(1)
            .with(
                function(path_eq("b_in")),
                function(path_eq("b_out")),
                eq(None),
            )
            .in_sequence(&mut seq)
            .returning(|_, _, _| Ok(()));

        actions[1]
            .run(&mut fs, &options, &handlebars, &variables)
            .unwrap();
    }
}
