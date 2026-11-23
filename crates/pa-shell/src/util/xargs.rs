//! `xargs [-n COUNT] [-I TEXT] [-0] [-d DELIMITER] [-r] [COMMAND [ARGUMENT...]]`.
//!
//! The command is run by the shell that runs `xargs`, so it may be a
//! builtin, one of the shell's utilities, or a program; its input is empty,
//! as under GNU xargs. Options not listed here are the program's.

use std::io::{Read, Write};

use brush_core::{CommandArg, ExecutionContext, ExecutionResult};

use super::{embedding, hand_over};
use crate::engine::Extensions;
use crate::STOPPED_EXIT_CODE;

/// The most bytes of arguments one command gets (GNU's default).
const COMMAND_BYTES: usize = 128 * 1024;

pub(super) async fn xargs(
    context: ExecutionContext<'_, Extensions>,
    args: Vec<CommandArg>,
) -> Result<ExecutionResult, brush_core::Error> {
    let words: Vec<String> = args.iter().skip(1).map(ToString::to_string).collect();
    let (mut per_command, mut replace, mut delimiter): (Option<usize>, Option<String>, Option<u8>) =
        (None, None, None);
    let mut run_if_empty = true;
    let mut at = 0;
    let mut unsupported = None;
    while let Some(word) = words
        .get(at)
        .filter(|word| word.starts_with('-') && word.len() > 1)
    {
        at += 1;
        let (option, attached) = if word.starts_with("--") {
            (word.as_str(), None)
        } else {
            word.split_at_checked(2)
                .map_or((word.as_str(), None), |(option, rest)| {
                    (option, Some(rest).filter(|rest| !rest.is_empty()))
                })
        };
        let mut value = || {
            attached.map(str::to_string).or_else(|| {
                at += 1;
                words.get(at - 1).cloned()
            })
        };
        match option {
            "-n" => {
                per_command = value()
                    .and_then(|count| count.parse().ok())
                    .filter(|count| *count > 0);
            }
            "-I" => replace = value(),
            "-0" | "--null" => delimiter = Some(0),
            "-d" => {
                delimiter = match value().as_deref() {
                    Some("\\n") => Some(b'\n'),
                    Some("\\t") => Some(b'\t'),
                    Some("\\0") => Some(0),
                    Some(text) if text.len() == 1 => text.bytes().next(),
                    Some(_) | None => None,
                };
                if delimiter.is_none() {
                    unsupported = Some("this -d delimiter".to_string());
                }
            }
            "-r" | "--no-run-if-empty" => run_if_empty = false,
            "--" => break,
            other => unsupported = Some(format!("option {other}")),
        }
        if (option == "-n" && per_command.is_none()) || (option == "-I" && replace.is_none()) {
            unsupported = Some(format!("{option} without a usable value"));
        }
    }
    if let Some(what) = unsupported {
        return hand_over(&context, "xargs", &words, &what).await;
    }
    let command: Vec<String> = if at < words.len() {
        words[at..].to_vec()
    } else {
        vec!["echo".to_string()]
    };
    let mut input = Vec::new();
    context.stdin().read_to_end(&mut input)?;
    let text = String::from_utf8_lossy(&input);
    // The items: whole lines for -I, split at the delimiter when one is
    // given, otherwise blank-separated words with quotes and backslashes.
    let items: Vec<String> = if let Some(delimiter) = delimiter {
        text.split(char::from(delimiter))
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect()
    } else if replace.is_some() {
        text.lines()
            .map(|line| line.trim_start_matches([' ', '\t']).to_string())
            .filter(|line| !line.is_empty())
            .collect()
    } else {
        let mut items = Vec::new();
        let mut current: Option<String> = None;
        let mut chars = text.chars();
        while let Some(character) = chars.next() {
            match character {
                ' ' | '\t' | '\n' => items.extend(current.take()),
                '\'' | '"' => {
                    let item = current.get_or_insert_with(String::new);
                    for quoted in chars.by_ref() {
                        if quoted == character {
                            break;
                        }
                        item.push(quoted);
                    }
                }
                '\\' => current.get_or_insert_with(String::new).extend(chars.next()),
                other => current.get_or_insert_with(String::new).push(other),
            }
        }
        items.extend(current);
        items
    };
    let quote = |word: &str| format!("'{}'", word.replace('\'', "'\\''"));
    // Each command as shell text, every word quoted.
    let mut lines: Vec<String> = Vec::new();
    if let Some(placeholder) = &replace {
        for item in &items {
            let replaced: Vec<String> = command
                .iter()
                .map(|word| quote(&word.replace(placeholder, item)))
                .collect();
            lines.push(replaced.join(" "));
        }
    } else {
        let prefix: Vec<String> = command.iter().map(|word| quote(word)).collect();
        let prefix = prefix.join(" ");
        let mut batch = prefix.clone();
        let mut count = 0;
        for item in &items {
            let quoted = quote(item);
            if count > 0
                && (per_command.is_some_and(|limit| count >= limit)
                    || batch.len() + quoted.len() + 1 > COMMAND_BYTES)
            {
                lines.push(std::mem::replace(&mut batch, prefix.clone()));
                count = 0;
            }
            batch.push(' ');
            batch.push_str(&quoted);
            count += 1;
        }
        if count > 0 || (items.is_empty() && run_if_empty) {
            lines.push(batch);
        }
    }
    if let Some(embedding) = embedding(&context) {
        embedding
            .in_process
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let mut params = context.params.clone();
    params.set_fd(0, brush_core::openfiles::null()?);
    let mut status = 0;
    let shell = context.shell;
    for line in lines {
        if shell.host().is_some_and(|host| host.interrupted()) {
            return Ok(ExecutionResult::new(
                u8::try_from(STOPPED_EXIT_CODE).unwrap_or(130),
            ));
        }
        let code = match shell
            .run_string(line, &brush_core::SourceInfo::default(), &params)
            .await
        {
            Ok(result) => u8::from(result.exit_code),
            Err(error) => {
                let mut stderr = params.stderr(shell);
                shell.display_error(&mut stderr, &error)?;
                u8::from(error.into_result(shell).exit_code)
            }
        };
        match code {
            0 => {}
            255 => {
                writeln!(
                    params.stderr(shell),
                    "xargs: {}: exited with status 255; aborting",
                    command[0]
                )?;
                return Ok(ExecutionResult::new(124));
            }
            126 | 127 => return Ok(ExecutionResult::new(code)),
            _ => status = 123,
        }
    }
    Ok(ExecutionResult::new(status))
}
