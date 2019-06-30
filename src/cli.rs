use crate::commands::autoview;
use crate::commands::classified::SinkCommand;
use crate::commands::command::sink;

use crate::prelude::*;

use crate::commands::classified::{
    ClassifiedCommand, ClassifiedInputStream, ClassifiedPipeline, ExternalCommand, InternalCommand,
    StreamNext,
};
use crate::context::Context;
crate use crate::errors::ShellError;
use crate::evaluate::Scope;
use crate::parser::parse::span::Spanned;
use crate::parser::registry;
use crate::parser::{Pipeline, PipelineElement, TokenNode};

use crate::git::current_branch;
use crate::object::Value;

use log::{debug, trace};
use rustyline::error::ReadlineError;
use rustyline::{self, ColorMode, Config, Editor};

use std::error::Error;
use std::iter::Iterator;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
pub enum MaybeOwned<'a, T> {
    Owned(T),
    Borrowed(&'a T),
}

impl<T> MaybeOwned<'a, T> {
    crate fn borrow(&self) -> &T {
        match self {
            MaybeOwned::Owned(v) => v,
            MaybeOwned::Borrowed(v) => v,
        }
    }
}

pub async fn cli() -> Result<(), Box<dyn Error>> {
    let mut context = Context::basic()?;

    {
        use crate::commands::*;

        context.add_commands(vec![
            command("ps", ps::ps),
            command("ls", ls::ls),
            command("sysinfo", sysinfo::sysinfo),
            command("cd", cd::cd),
            command("view", view::view),
            command("skip", skip::skip),
            command("first", first::first),
            command("size", size::size),
            command("from-ini", from_ini::from_ini),
            command("from-json", from_json::from_json),
            command("from-toml", from_toml::from_toml),
            command("from-xml", from_xml::from_xml),
            command("from-yaml", from_yaml::from_yaml),
            command("get", get::get),
            command("enter", enter::enter),
            command("exit", exit::exit),
            command("lines", lines::lines),
            command("pick", pick::pick),
            command("split-column", split_column::split_column),
            command("split-row", split_row::split_row),
            command("lines", lines::lines),
            command("reject", reject::reject),
            command("trim", trim::trim),
            command("to-array", to_array::to_array),
            command("to-json", to_json::to_json),
            command("to-toml", to_toml::to_toml),
            command("sort-by", sort_by::sort_by),
            Arc::new(Open),
            Arc::new(Where),
            Arc::new(Config),
            Arc::new(SkipWhile),
            command("sort-by", sort_by::sort_by),
            command("inc", |x| plugin::plugin("inc".into(), x)),
            command("sum", |x| plugin::plugin("sum".into(), x)),
        ]);

        context.add_sinks(vec![
            sink("autoview", autoview::autoview),
            sink("clip", clip::clip),
            sink("save", save::save),
            sink("table", table::table),
            sink("tree", tree::tree),
            sink("vtable", vtable::vtable),
        ]);
    }

    let config = Config::builder().color_mode(ColorMode::Forced).build();
    let h = crate::shell::Helper::new(context.clone_commands());
    let mut rl: Editor<crate::shell::Helper> = Editor::with_config(config);

    #[cfg(windows)]
    {
        let _ = ansi_term::enable_ansi_support();
    }

    rl.set_helper(Some(h));
    let _ = rl.load_history("history.txt");

    let ctrl_c = Arc::new(AtomicBool::new(false));
    let cc = ctrl_c.clone();
    ctrlc::set_handler(move || {
        cc.store(true, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");
    let mut ctrlcbreak = false;
    loop {
        if ctrl_c.load(Ordering::SeqCst) {
            ctrl_c.store(false, Ordering::SeqCst);
            continue;
        }

        let (obj, cwd) = {
            let env = context.env.lock().unwrap();
            let last = env.back().unwrap();
            (last.obj().clone(), last.path().display().to_string())
        };
        let readline = match obj {
            Value::Filesystem => rl.readline(&format!(
                "{}{}> ",
                cwd,
                match current_branch() {
                    Some(s) => format!("({})", s),
                    None => "".to_string(),
                }
            )),
            _ => rl.readline(&format!("{}{}> ", obj.type_name(), cwd)),
        };

        match process_line(readline, &mut context).await {
            LineResult::Success(line) => {
                rl.add_history_entry(line.clone());
            }

            LineResult::CtrlC => {
                if ctrlcbreak {
                    std::process::exit(0);
                } else {
                    context
                        .host
                        .lock()
                        .unwrap()
                        .stdout("CTRL-C pressed (again to quit)");
                    ctrlcbreak = true;
                    continue;
                }
            }

            LineResult::Error(mut line, err) => {
                rl.add_history_entry(line.clone());

                let diag = err.to_diagnostic();
                let host = context.host.lock().unwrap();
                let writer = host.err_termcolor();
                line.push_str(" ");
                let files = crate::parser::Files::new(line);

                language_reporting::emit(
                    &mut writer.lock(),
                    &files,
                    &diag,
                    &language_reporting::DefaultConfig,
                )
                .unwrap();
            }

            LineResult::Break => {
                break;
            }

            LineResult::FatalError(_, err) => {
                context
                    .host
                    .lock()
                    .unwrap()
                    .stdout(&format!("A surprising fatal error occurred.\n{:?}", err));
            }
        }
        ctrlcbreak = false;
    }
    rl.save_history("history.txt").unwrap();

    Ok(())
}

enum LineResult {
    Success(String),
    Error(String, ShellError),
    CtrlC,
    Break,

    #[allow(unused)]
    FatalError(String, ShellError),
}

impl std::ops::Try for LineResult {
    type Ok = Option<String>;
    type Error = (String, ShellError);

    fn into_result(self) -> Result<Option<String>, (String, ShellError)> {
        match self {
            LineResult::Success(s) => Ok(Some(s)),
            LineResult::Error(string, err) => Err((string, err)),
            LineResult::Break => Ok(None),
            LineResult::CtrlC => Ok(None),
            LineResult::FatalError(string, err) => Err((string, err)),
        }
    }
    fn from_error(v: (String, ShellError)) -> Self {
        LineResult::Error(v.0, v.1)
    }

    fn from_ok(v: Option<String>) -> Self {
        match v {
            None => LineResult::Break,
            Some(v) => LineResult::Success(v),
        }
    }
}

async fn process_line(readline: Result<String, ReadlineError>, ctx: &mut Context) -> LineResult {
    match &readline {
        Ok(line) if line.trim() == "" => LineResult::Success(line.clone()),

        Ok(line) => {
            let result = match crate::parser::parse(&line) {
                Err(err) => {
                    return LineResult::Error(line.clone(), err);
                }

                Ok(val) => val,
            };

            debug!("=== Parsed ===");
            debug!("{:#?}", result);

            let mut pipeline = classify_pipeline(&result, ctx, &Text::from(line))
                .map_err(|err| (line.clone(), err))?;

            match pipeline.commands.last() {
                Some(ClassifiedCommand::Sink(_)) => {}
                Some(ClassifiedCommand::External(_)) => {}
                _ => pipeline.commands.push(ClassifiedCommand::Sink(SinkCommand {
                    command: sink("autoview", autoview::autoview),
                    name_span: None,
                    args: registry::Args {
                        positional: None,
                        named: None,
                    },
                })),
            }

            let mut input = ClassifiedInputStream::new();

            let mut iter = pipeline.commands.into_iter().peekable();

            loop {
                let item: Option<ClassifiedCommand> = iter.next();
                let next: Option<&ClassifiedCommand> = iter.peek();

                input = match (item, next) {
                    (None, _) => break,

                    (Some(ClassifiedCommand::Expr(_)), _) => {
                        return LineResult::Error(
                            line.clone(),
                            ShellError::unimplemented("Expression-only commands"),
                        )
                    }

                    (_, Some(ClassifiedCommand::Expr(_))) => {
                        return LineResult::Error(
                            line.clone(),
                            ShellError::unimplemented("Expression-only commands"),
                        )
                    }

                    (Some(ClassifiedCommand::Sink(SinkCommand { name_span, .. })), Some(_)) => {
                        return LineResult::Error(line.clone(), ShellError::maybe_labeled_error("Commands like table, save, and autoview must come last in the pipeline", "must come last", name_span));
                    }

                    (Some(ClassifiedCommand::Sink(left)), None) => {
                        let input_vec: Vec<Value> = input.objects.collect().await;
                        if let Err(err) = left.run(ctx, input_vec) {
                            return LineResult::Error(line.clone(), err);
                        }
                        break;
                    }

                    (
                        Some(ClassifiedCommand::Internal(left)),
                        Some(ClassifiedCommand::External(_)),
                    ) => match left.run(ctx, input).await {
                        Ok(val) => ClassifiedInputStream::from_input_stream(val),
                        Err(err) => return LineResult::Error(line.clone(), err),
                    },

                    (Some(ClassifiedCommand::Internal(left)), Some(_)) => {
                        match left.run(ctx, input).await {
                            Ok(val) => ClassifiedInputStream::from_input_stream(val),
                            Err(err) => return LineResult::Error(line.clone(), err),
                        }
                    }

                    (Some(ClassifiedCommand::Internal(left)), None) => {
                        match left.run(ctx, input).await {
                            Ok(val) => ClassifiedInputStream::from_input_stream(val),
                            Err(err) => return LineResult::Error(line.clone(), err),
                        }
                    }

                    (
                        Some(ClassifiedCommand::External(left)),
                        Some(ClassifiedCommand::External(_)),
                    ) => match left.run(ctx, input, StreamNext::External).await {
                        Ok(val) => val,
                        Err(err) => return LineResult::Error(line.clone(), err),
                    },

                    (Some(ClassifiedCommand::External(left)), Some(_)) => {
                        match left.run(ctx, input, StreamNext::Internal).await {
                            Ok(val) => val,
                            Err(err) => return LineResult::Error(line.clone(), err),
                        }
                    }

                    (Some(ClassifiedCommand::External(left)), None) => {
                        match left.run(ctx, input, StreamNext::Last).await {
                            Ok(val) => val,
                            Err(err) => return LineResult::Error(line.clone(), err),
                        }
                    }
                }
            }

            LineResult::Success(line.clone())
        }
        Err(ReadlineError::Interrupted) => LineResult::CtrlC,
        Err(ReadlineError::Eof) => {
            println!("CTRL-D");
            LineResult::Break
        }
        Err(err) => {
            println!("Error: {:?}", err);
            LineResult::Break
        }
    }
}

fn classify_pipeline(
    pipeline: &TokenNode,
    context: &Context,
    source: &Text,
) -> Result<ClassifiedPipeline, ShellError> {
    let pipeline = pipeline.as_pipeline()?;

    let Pipeline { parts, .. } = pipeline;

    let commands: Result<Vec<_>, ShellError> = parts
        .iter()
        .map(|item| classify_command(&item, context, &source))
        .collect();

    Ok(ClassifiedPipeline {
        commands: commands?,
    })
}

fn classify_command(
    command: &PipelineElement,
    context: &Context,
    source: &Text,
) -> Result<ClassifiedCommand, ShellError> {
    let call = command.call();

    match call {
        call if call.head().is_bare() => {
            let head = call.head();
            let name = head.source(source);

            match context.has_command(name) {
                true => {
                    let command = context.get_command(name);
                    let config = command.config();
                    let scope = Scope::empty();

                    trace!("classifying {:?}", config);

                    let args = config.evaluate_args(call, context, &scope, source)?;

                    Ok(ClassifiedCommand::Internal(InternalCommand {
                        command,
                        name_span: Some(head.span().clone()),
                        args,
                    }))
                }
                false => match context.has_sink(name) {
                    true => {
                        let command = context.get_sink(name);
                        let config = command.config();
                        let scope = Scope::empty();

                        let args = config.evaluate_args(call, context, &scope, source)?;

                        Ok(ClassifiedCommand::Sink(SinkCommand {
                            command,
                            name_span: Some(head.span().clone()),
                            args,
                        }))
                    }
                    false => {
                        let arg_list_strings: Vec<Spanned<String>> = match call.children() {
                            //Some(args) => args.iter().map(|i| i.as_external_arg(source)).collect(),
                            Some(args) => args
                                .iter()
                                .filter_map(|i| match i {
                                    TokenNode::Whitespace(_) => None,
                                    other => Some(Spanned::from_item(
                                        other.as_external_arg(source),
                                        other.span(),
                                    )),
                                })
                                .collect(),
                            None => vec![],
                        };

                        Ok(ClassifiedCommand::External(ExternalCommand {
                            name: name.to_string(),
                            name_span: Some(head.span().clone()),
                            args: arg_list_strings,
                        }))
                    }
                },
            }
        }

        call => Err(ShellError::diagnostic(
            language_reporting::Diagnostic::new(
                language_reporting::Severity::Error,
                "Invalid command",
            )
            .with_label(language_reporting::Label::new_primary(call.head().span())),
        )),
    }
}
