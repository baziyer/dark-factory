use std::io::Write;

use factory_core::local::{LocalRequest, MAX_EVENT_PAGE_ITEMS};
use factoryctl::Client;

use super::CliCommand;

pub(super) const HELP: &str = "usage: factoryctl events [--after N] [--limit N] [--follow]

Read durable events from the daemon.

Options:
  --after N                Read events after this sequence (default 0)
  --limit N                 Page size (default and max: 16; not with --follow)
  --follow                   Stream events as they occur
  -h, --help                  Show this help";

const EVENT_LIST_LIMIT: u32 = MAX_EVENT_PAGE_ITEMS;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Command {
    after_sequence: i64,
    limit: u32,
    follow: bool,
}

pub(super) fn parse(args: Vec<String>) -> Result<CliCommand, String> {
    if super::wants_help(&args) {
        return Ok(CliCommand::Help(HELP));
    }
    parse_command(args).map(CliCommand::Events)
}

fn parse_command(mut args: Vec<String>) -> Result<Command, String> {
    let after_sequence = super::take_option(&mut args, "--after")?
        .map(|value| super::parse_number(&value, "--after"))
        .transpose()?
        .unwrap_or(0);
    if after_sequence < 0 {
        return Err("--after must be zero or greater".into());
    }
    let (limit, explicit_limit) =
        super::take_limit(&mut args, EVENT_LIST_LIMIT, MAX_EVENT_PAGE_ITEMS)?;
    let follow = super::take_flag(&mut args, "--follow")?;
    if follow && explicit_limit {
        return Err("--limit cannot be used with --follow".into());
    }
    super::require_empty(&args)?;
    Ok(Command {
        after_sequence,
        limit,
        follow,
    })
}

pub(super) fn request(command: Command) -> LocalRequest {
    if command.follow {
        LocalRequest::Subscribe {
            after_sequence: command.after_sequence,
        }
    } else {
        LocalRequest::EventsAfter {
            sequence: command.after_sequence,
            limit: command.limit,
        }
    }
}

pub(super) fn run_follow(
    client: &Client,
    command: &Command,
    output: &mut impl Write,
) -> Result<Option<i32>, String> {
    if !command.follow {
        return Ok(None);
    }
    for frame in client
        .subscribe(command.after_sequence)
        .map_err(|error| error.to_string())?
    {
        let frame = frame.map_err(|error| error.to_string())?;
        super::write_frame(output, &frame)?;
        if super::is_error(&frame) {
            return Ok(Some(2));
        }
    }
    Ok(Some(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn follow_is_an_explicit_subscription() {
        let command = parse(args(&["--after", "12", "--follow"])).unwrap();
        assert_eq!(
            command,
            CliCommand::Events(Command {
                after_sequence: 12,
                limit: EVENT_LIST_LIMIT,
                follow: true,
            })
        );
    }

    #[test]
    fn follow_rejects_an_explicit_limit() {
        let error = parse(args(&["--follow", "--limit", "1"])).unwrap_err();
        assert_eq!(error, "--limit cannot be used with --follow");
    }
}
