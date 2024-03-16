mod options;

use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, BufWriter, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::{Duration, SystemTime},
};

use anyhow::Context;
use clap::{crate_name, CommandFactory, Parser};
use console::Term;
use dialoguer::Confirm;
use humantime::format_duration;
use options::{Interactive, Options, RunOptions};

fn main() -> anyhow::Result<()> {
    let carte_name = crate_name!();

    let mut builder = pretty_env_logger::formatted_builder();
    let filters = match std::env::var("RUST_LOG") {
        Ok(f) => f,
        Err(_) => format!("{carte_name}=info"),
    };
    builder.parse_filters(&filters);
    builder.try_init()?;

    let options = Options::parse();

    match options.command {
        options::Commands::Run(run_opts) => {
            let context = RunContext::new(run_opts)?;
            context.run()?;
            context.finish()
        }
        options::Commands::Completion(gen_options) => {
            generate_shell_completions(gen_options, carte_name)
        }
    }
}

#[derive(Debug)]
struct RunContext {
    options: RunOptions,
    uid: u32,
    now: SystemTime,
    term: Term,
    output: Option<Mutex<Output>>,
    statistic: Statistics,
}

#[derive(Debug, Clone, Copy)]
enum Action {
    Remove,
    AboutToRemove,
}

#[derive(Debug, Clone)]
struct Reason {
    target: PathBuf,
    elapsed: Duration,
}

#[derive(Debug, Clone)]
struct ToRemove<'c> {
    context: &'c RunContext,
    link_path: PathBuf,
    reason: Reason,
}

#[derive(Debug, Default)]
struct Statistics {
    traversed: Counter,
    candidate: Counter,
    invalid: Counter,
    removed: Counter,
}

#[derive(Debug)]
struct Output {
    writer: BufWriter<File>,
    first_output: bool,
}

#[derive(Debug, Default)]
struct Counter(AtomicUsize);

impl RunContext {
    fn new(options: RunOptions) -> anyhow::Result<Self> {
        let uid = uzers::get_current_uid();
        let now = SystemTime::now();
        let term = Term::stderr();
        let output = match &options.output {
            Some(path) => {
                let writer = BufWriter::new(
                    File::create(path)
                        .with_context(|| format!("failed to create output file {path:?}"))?,
                );
                Some(Mutex::new(Output {
                    writer,
                    first_output: true,
                }))
            }
            None => None,
        };
        let statistic = Default::default();
        let context = Self {
            options,
            uid,
            now,
            term,
            output,
            statistic,
        };
        log::debug!("options: {:#?}", context.options);
        Ok(context)
    }

    fn run(&self) -> anyhow::Result<()> {
        let mut waiting = Vec::new();

        for path in &self.options.directory {
            let directory =
                fs::read_dir(path).with_context(|| format!("failed to open directory {path:?}"))?;
            for entry in directory {
                self.statistic.traversed.increase();
                let link = entry.with_context(|| {
                    format!("failed to read directory entry from directory {path:?}")
                })?;
                let link_path = link.path();
                match self.check(&link_path)? {
                    Some(reason) => {
                        self.statistic.candidate.increase();
                        let to_remove = ToRemove {
                            context: self,
                            link_path,
                            reason,
                        };
                        match self.options.interactive {
                            Interactive::Always => {
                                to_remove.notify(Action::AboutToRemove, true)?;
                                let yes = self.prompt()?;
                                if yes {
                                    to_remove.notify(Action::Remove, false)?;
                                    to_remove.remove()?;
                                }
                            }
                            Interactive::Once => {
                                to_remove.notify(Action::AboutToRemove, true)?;
                                waiting.push(to_remove);
                            }
                            Interactive::Never => {
                                to_remove.notify(Action::Remove, true)?;
                                to_remove.remove()?;
                            }
                        }
                    }
                    None => log::trace!("keep {link_path:?}"),
                }
            }
        }

        if !waiting.is_empty() && self.prompt()? {
            for to_remove in &waiting {
                to_remove.notify(Action::Remove, false)?;
                to_remove.remove()?;
            }
        }

        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<()> {
        if !self.options.no_statistic {
            writeln!(
                self.term,
                "{}",
                self.term.style().bold().underlined().apply_to("Statistics")
            )?;
            self.term
                .write_line(&self.statistic.format_with_style(&self.term))?;
        }
        Ok(())
    }

    fn check<P: AsRef<Path>>(&self, link_path: P) -> anyhow::Result<Option<Reason>> {
        let link_path = link_path.as_ref();
        let target = fs::read_link(link_path)
            .with_context(|| format!("failed to read symbolic link {link_path:?}"))?;
        log::trace!("processing {link_path:?} -> {target:?}");
        let metadata = match fs::symlink_metadata(&target) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                log::debug!("target of {link_path:?} not found, skip");
                return Ok(None);
            }
            e => e.with_context(|| format!("failed to read metadata of file {target:?}"))?,
        };
        if self.options.owned_only {
            let file_uid = metadata.uid();
            if file_uid != self.uid {
                log::debug!("ignore {target:?} due to uid mismatch: file uid == {file_uid}, process uid == {process_uid}",
                  process_uid = self.uid);
                return Ok(None);
            }
        }
        let target_mtime = metadata
            .modified()
            .with_context(|| format!("failed to get modified time of file {target:?}"))?;
        let elapsed = self
            .now
            .duration_since(target_mtime)
            .unwrap_or_else(|_| Duration::new(0, 0));
        log::trace!("elapsed: {}", humantime::format_duration(elapsed));
        if elapsed <= self.options.period {
            return Ok(None);
        }

        Ok(Some(Reason { target, elapsed }))
    }

    fn validate<P: AsRef<Path>>(&self, target: P) -> anyhow::Result<bool> {
        let target = target.as_ref();
        let final_target = fs::canonicalize(target)
            .with_context(|| format!("failed to canonicalize {target:?} for validation"))?;
        Ok(final_target.starts_with(&self.options.store))
    }

    fn validate_and_prompt<P: AsRef<Path>>(&self, target: P) -> anyhow::Result<bool> {
        let target = target.as_ref();
        if !self.validate(target)? {
            self.statistic.invalid.increase();
            let mut term = self.term.clone();
            let fail_message_style = if self.options.force {
                term.style().bold().yellow()
            } else {
                term.style().bold().red()
            };
            writeln!(
                term,
                "{}, target {:?} does not point into store {:?}",
                fail_message_style.apply_to("Validation failed"),
                term.style().underlined().apply_to(&target),
                self.options.store
            )?;
            let notify = |action| {
                writeln!(
                    term.clone(),
                    "> {}",
                    term.style().bold().yellow().apply_to(action)
                )
            };
            let notify_then_continue = || {
                notify("continue")?;
                Ok(true)
            };
            let notify_then_ignored = || {
                notify("ignore")?;
                Ok(false)
            };
            if self.options.force {
                notify_then_continue()
            } else if self.options.interactive == Interactive::Never {
                notify_then_ignored()
            } else if self.prompt()? {
                notify_then_continue()
            } else {
                notify_then_ignored()
            }
        } else {
            Ok(true)
        }
    }

    fn prompt(&self) -> anyhow::Result<bool> {
        Confirm::new()
            .with_prompt("Do you want to continue?")
            .report(false)
            .interact_on(&self.term)
            .context("failed to prompt")
    }
}

impl<'c> ToRemove<'c> {
    fn options(&self) -> &RunOptions {
        &self.context.options
    }

    fn notify(&self, action: Action, with_reason: bool) -> anyhow::Result<()> {
        let mut term = self.context.term.clone();
        let reason_indent = 2;
        if self.options().remove_root {
            // remove link
            writeln!(
                term,
                "{} {:?}",
                action.format_with_style(&term),
                self.link_path
            )?;
            if with_reason {
                term.write_line(&add_indent(
                    &self.reason.format_with_style(&term),
                    reason_indent,
                ))?;
            }
        } else {
            // remove target
            writeln!(
                term,
                "{} {:?}",
                action.format_with_style(&term),
                self.reason.target
            )?;
            if with_reason {
                term.write_line(&add_indent(
                    &self.reason.format_with_style_no_target(&term),
                    reason_indent,
                ))?;
            }
        }
        Ok(())
    }

    fn remove(&self) -> anyhow::Result<()> {
        let path_to_remove = if self.options().remove_root {
            &self.link_path
        } else {
            // validate before remove target
            let target = &self.reason.target;
            if !self.context.validate_and_prompt(target)? {
                return Ok(());
            }
            target
        };
        if !self.options().dry_run {
            fs::remove_file(path_to_remove)
                .with_context(|| format!("failed to remove {:?}", path_to_remove))?;
        }
        self.context.statistic.removed.increase();
        if let Some(output) = &self.context.output {
            let mut out = output.lock().unwrap();
            out.output(path_to_remove, &self.options().output_delimiter)?;
        }
        Ok(())
    }
}

impl Action {
    fn format_with_style(&self, term: &Term) -> String {
        match self {
            Action::Remove => term.style().green().bold().apply_to("Remove").to_string(),
            Action::AboutToRemove => term
                .style()
                .blue()
                .bold()
                .apply_to("About to remove")
                .to_string(),
        }
    }
}

impl Reason {
    fn format_with_style(&self, term: &Term) -> String {
        let Self { target, elapsed } = self;
        format!(
            "target {:?}\nwas last modified {} ago",
            term.style().underlined().apply_to(target),
            term.style().bold().apply_to(format_duration(*elapsed))
        )
    }

    fn format_with_style_no_target(&self, term: &Term) -> String {
        let Self { elapsed, .. } = self;
        format!(
            "was last modified {} ago",
            term.style().bold().apply_to(format_duration(*elapsed))
        )
    }
}

impl Statistics {
    fn format_with_style(self, term: &Term) -> String {
        let traversed = self.traversed.done();
        let candidate = self.candidate.done();
        let removed = self.removed.done();
        let invalid = self.invalid.done();
        let kept = traversed - removed;
        let num_style = |n| term.style().bold().apply_to(n);
        [
            format!("traversed: {}", num_style(traversed)),
            format!("candidate: {}", num_style(candidate)),
            format!("removed:   {}", num_style(removed)),
            format!("invalid:   {}", num_style(invalid)),
            format!("kept:      {}", num_style(kept)),
        ]
        .join("\n")
    }
}

impl Counter {
    fn increase(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn done(self) -> usize {
        self.0.into_inner()
    }
}

impl Output {
    fn output<P: AsRef<Path>>(&mut self, path: P, delimiter: &OsStr) -> anyhow::Result<()> {
        let p = path.as_ref();
        if !self.first_output {
            self.writer.write_all(delimiter.as_bytes())?;
        } else {
            self.first_output = false;
        }
        self.writer.write_all(p.as_os_str().as_bytes())?;
        Ok(())
    }
}

fn add_indent(text: &str, indent: usize) -> String {
    let indented_lines: Vec<_> = text
        .lines()
        .map(|l| format!("{:indent$}{}", "", l))
        .collect();
    indented_lines.join("\n")
}

fn generate_shell_completions(
    gen_options: options::CompletionOptions,
    command_name: &str,
) -> anyhow::Result<()> {
    let mut cli = options::Options::command();
    let mut stdout = std::io::stdout();
    clap_complete::generate(gen_options.shell, &mut cli, command_name, &mut stdout);
    Ok(())
}
