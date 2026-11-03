//! Remote hosts: what a host description may contain, where a remote worktree lives, and the one
//! place where a command line for a remote shell is assembled.
//!
//! Everything here is a pure function of its arguments. The rules, in short:
//!
//! * Host addresses, user names, ports and file names that end up in an `ssh` command line are
//!   validated against narrow character sets; nothing that could be read as an option, a
//!   `user@host:port` separator or an ssh_config token gets through.
//! * A remote worktree is `<base>/<run>/<task>` with run and task ids that are already safe path
//!   components, below a configured absolute base directory without `.` or `..` components.
//! * A remote command line is built only from a [`RemoteCall`]: a closed set of calls. Each is
//!   `/usr/bin/env -i <explicit environment> <program> ...` where the program is `git`, the
//!   worker command the operator allowed for that host, or `/bin/sh -c <constant script>` with
//!   the data passed as positional parameters. Every word goes through [`quote`], which makes
//!   the remote login shell see exactly one word per argument whatever the argument contains.
//!   (The remote login shell must read POSIX single quotes: sh, bash, dash, zsh, ksh. `fleet host
//!   doctor` checks this with a round trip of hostile text.)

use crate::env::is_valid_name;
use crate::error::{FleetError, Result};
use crate::ids::{show, RunId, Sha, TaskId};

/// Longest remote command line, in bytes. Longer than any shell accepts happily and than any
/// worker brief that should be sent on a command line (use `{brief_file}` for long briefs).
pub const MAX_REMOTE_COMMAND: usize = 64 * 1024;
/// Most environment variables sent to a remote worker.
pub const MAX_REMOTE_ENV: usize = 64;
/// Longest value of one such variable.
pub const MAX_REMOTE_ENV_VALUE: usize = 4096;
/// Most arguments of a remote worker.
pub const MAX_REMOTE_ARGS: usize = 256;
/// `PATH` of remote git and helper calls unless the host configures one.
pub const DEFAULT_REMOTE_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
/// The pty of a remote worker identifies itself as this terminal.
pub const REMOTE_TERM: &str = "xterm-256color";
/// Longest file the remote side sends back as a worker contract file (the local reader enforces
/// the exact limit; this keeps the remote `head` honest).
pub const REMOTE_CONTRACT_READ: usize = 64 * 1024 + 1;

const SH: &str = "/bin/sh";
const ENV: &str = "/usr/bin/env";
/// `$0` of every script.
const SCRIPT_NAME: &str = "fleet-remote";

// ---- validation of host settings -----------------------------------------------------------

fn bad(what: &str, value: &str, why: &str) -> FleetError {
    FleetError::invalid(format!("{what} {} {why}", show(value)))
}

/// A DNS name, an IPv4 address or an IPv6 literal without zone: the characters of a host name,
/// never an option (`-oProxyCommand=...`), a path or a `user@` prefix.
pub fn validate_address(address: &str) -> Result<()> {
    if address.is_empty() || address.len() > 253 {
        return Err(bad("host address", address, "must be 1-253 characters"));
    }
    if address.contains(':') {
        let ok = address.len() >= 2
            && address.len() <= 45
            && address.matches(':').count() >= 2
            && address.chars().any(|c| c.is_ascii_hexdigit())
            && address
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
        return if ok {
            Ok(())
        } else {
            Err(bad("host address", address, "is not an IPv6 literal"))
        };
    }
    for label in address.split('.') {
        let ok = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !ok {
            return Err(bad(
                "host address",
                address,
                "is not a host name or an IP address (letters, digits, '-' and '.')",
            ));
        }
    }
    Ok(())
}

/// A login name: `[A-Za-z_][A-Za-z0-9_.-]{0,31}`.
pub fn validate_user(user: &str) -> Result<()> {
    let mut chars = user.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    if !first_ok
        || user.len() > 32
        || !user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        return Err(bad(
            "remote user",
            user,
            "must match [A-Za-z_][A-Za-z0-9_.-]{0,31}",
        ));
    }
    Ok(())
}

pub fn validate_port(port: u32) -> Result<u16> {
    match u16::try_from(port) {
        Ok(p) if p != 0 => Ok(p),
        _ => Err(FleetError::invalid("the port must be between 1 and 65535")),
    }
}

/// A file on this machine that is named in an ssh option (identity file, known hosts file): an
/// absolute path of plain components and a narrow character set. ssh expands `%` and `~` and
/// splits option values at white space, so none of those is accepted.
pub fn validate_local_path(what: &str, path: &str) -> Result<()> {
    if !path.starts_with('/') || path.len() > 300 {
        return Err(bad(
            what,
            path,
            "must be an absolute path of at most 300 bytes",
        ));
    }
    if !path.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '@' | '+' | ':' | ',' | '.' | '/' | '-' | '=')
    }) {
        return Err(bad(
            what,
            path,
            "may only contain letters, digits and _ @ + : , . = / - (no spaces, '~' or '%')",
        ));
    }
    if path.len() > 1
        && path[1..]
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(bad(what, path, "has an empty, '.' or '..' component"));
    }
    Ok(())
}

/// The remote base directory: absolute, plain components, at least one, no `..` and no `.`.
pub fn validate_remote_base(base: &str) -> Result<()> {
    validate_local_path("remote base directory", base)?;
    if base == "/" {
        return Err(bad("remote base directory", base, "must not be the root"));
    }
    Ok(())
}

/// The name of a program the host may run as a worker: a bare name or an absolute path, never
/// something that starts with `-`.
pub fn validate_remote_program(program: &str) -> Result<()> {
    let ok = !program.is_empty()
        && program.len() <= 200
        && !program.starts_with('-')
        && program
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '+' | '.' | '/' | '-'))
        && !program.split('/').any(|c| c == "..");
    if ok {
        Ok(())
    } else {
        Err(bad(
            "remote worker command",
            program,
            "must be a program name or absolute path of letters, digits and _ @ + . / -",
        ))
    }
}

/// One explicit variable for a remote worker: a valid name and a value without NUL, bounded.
pub fn validate_remote_env(name: &str, value: &str) -> Result<()> {
    if !is_valid_name(name) {
        return Err(bad(
            "remote environment variable name",
            name,
            "is not valid",
        ));
    }
    if value.contains(['\0', '\n']) || value.len() > MAX_REMOTE_ENV_VALUE {
        return Err(FleetError::invalid(format!(
            "the value of remote variable {name} is too long or contains NUL or a line break"
        )));
    }
    Ok(())
}

// ---- where things live remotely -----------------------------------------------------------------

/// The remote directories of one host, below a validated base directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteLayout {
    base: String,
}

impl RemoteLayout {
    pub fn new(base: &str) -> Result<RemoteLayout> {
        validate_remote_base(base)?;
        Ok(RemoteLayout {
            base: base.to_string(),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// `<base>/<run>/<task>`: the worktree of one task. Run and task ids are `[a-z0-9_-]+`, so
    /// the result cannot leave the base directory.
    pub fn worktree(&self, run: &RunId, task: &TaskId) -> String {
        format!("{}/{}/{}", self.base, run.as_str(), task.as_str())
    }

    /// The file the variables of the worker wait in until the launcher reads and deletes it.
    pub fn envfile(&self, run: &RunId, task: &TaskId) -> String {
        format!("{}/{}/{}.env", self.base, run.as_str(), task.as_str())
    }

    /// The file the launcher writes its process ids to, next to (not inside) the worktree.
    pub fn pidfile(&self, run: &RunId, task: &TaskId) -> String {
        format!("{}/{}/{}.pid", self.base, run.as_str(), task.as_str())
    }
}

// ---- quoting -----------------------------------------------------------------------------------

fn is_bare(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '%' | '+' | ':' | ',' | '.' | '/' | '-')
}

/// Quotes one word for a POSIX shell: words made only of harmless characters stay as they are,
/// everything else is wrapped in single quotes with every embedded single quote written as `'\''`.
/// Inside single quotes a POSIX shell interprets nothing, so any text (newlines, `$`, backticks,
/// `;`, `*`, `\`, `~`, `=`, non-ASCII) comes out as exactly this one word. A NUL cannot be
/// carried by a command line at all and is refused.
pub fn quote(word: &str) -> Result<String> {
    if word.contains('\0') {
        return Err(FleetError::invalid(
            "a remote command argument contains a NUL byte",
        ));
    }
    if !word.is_empty() && word.chars().all(is_bare) {
        return Ok(word.to_string());
    }
    let mut out = String::with_capacity(word.len() + 2);
    out.push('\'');
    for c in word.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    Ok(out)
}

fn join(words: &[String]) -> Result<String> {
    let line = words.join(" ");
    if line.len() > MAX_REMOTE_COMMAND {
        return Err(FleetError::limit(format!(
            "the remote command line is longer than {MAX_REMOTE_COMMAND} bytes (use {{brief_file}} instead of {{brief}} for long briefs)"
        )));
    }
    Ok(line)
}

// ---- the constant scripts --------------------------------------------------------------------------

/// Defines `g`, git with the same neutralised configuration the local git adapter uses.
const GIT_FN: &str = "g() { git -c core.hooksPath=/dev/null -c core.fsmonitor=false -c core.pager=cat -c core.editor=true -c core.sshCommand=false -c core.askPass=false -c core.attributesFile=/dev/null -c credential.helper= -c protocol.allow=never -c commit.gpgsign=false -c tag.gpgsign=false -c gc.auto=0 -c maintenance.auto=false -c diff.external= -c core.useReplaceRefs=false -c user.name=fleet -c user.email=fleet@localhost \"$@\"; }; ";

/// Script of [`RemoteCall::Prepare`]: `$1` base, `$2` worktree; the bundle on stdin.
const PREPARE: &str = "set -eu; umask 077; base=$1; dir=$2; \
case $dir in \"$base\"/?*/?*) ;; *) echo 'fleet: directory outside the base directory' >&2; exit 71;; esac; \
case $dir in */../*|*/./*|*/..|*/.) echo 'fleet: directory with . or ..' >&2; exit 71;; esac; \
mkdir -p -- \"$base\"; \
[ -d \"$base\" ] && [ ! -L \"$base\" ] || { echo 'fleet: the base directory is not a plain directory' >&2; exit 70; }; \
[ -O \"$base\" ] || { echo 'fleet: the base directory is not owned by this user' >&2; exit 70; }; \
p=$(ls -ld -- \"$base\" | cut -c1-10); case $p in ?????w????|????????w?) echo 'fleet: the base directory is writable by others' >&2; exit 70;; esac; \
run=${dir%/*}; mkdir -p -- \"$run\"; \
[ ! -L \"$run\" ] || { echo 'fleet: the run directory is a link' >&2; exit 70; }; \
[ -O \"$run\" ] || { echo 'fleet: the run directory is not owned by this user' >&2; exit 70; }; \
p=$(ls -ld -- \"$run\" | cut -c1-10); case $p in ?????w????|????????w?) echo 'fleet: the run directory is writable by others' >&2; exit 70;; esac; \
[ ! -e \"$dir\" ] || { echo 'fleet: the remote worktree exists already' >&2; exit 72; }; \
mkdir -- \"$dir\"; cd -- \"$dir\"; \
g init -q .; \
cat > .git/fleet-in.bundle; \
g -c protocol.file.allow=always fetch -q --no-tags .git/fleet-in.bundle '+refs/fleet-remote/*:refs/fleet-in/*'; \
sent=$(g for-each-ref --count=1 --format='%(objectname)' refs/fleet-in/); \
[ -n \"$sent\" ] || { echo 'fleet: the bundle holds no ref' >&2; exit 73; }; \
g update-ref refs/heads/fleet-sent \"$sent\"; \
rm -f .git/fleet-in.bundle; \
printf '.fleet/\\n' >> .git/info/exclude; \
g checkout -q -b work fleet-sent";

/// Script of [`RemoteCall::PutFile`]: `$1` worktree, `$2` the file name below `.fleet` (one of the
/// constants of [`InputFile::file_name`]); the file on stdin.
const PUT_FILE: &str = "set -eu; umask 077; cd -- \"$1\"; mkdir -p .fleet; cat > \".fleet/$2\"";

/// Script of [`RemoteCall::Launch`]: `$1` worktree, `$2` process id file, then the program and
/// its arguments. Records the process id and the process group (what a later signal needs) and
/// replaces itself with the worker.
const LAUNCH: &str = "cd -- \"$1\" || exit 126; pidfile=$2; envfile=$3; shift 3; \
pg=$(ps -o pgid= -p \"$$\" 2>/dev/null | tr -d ' '); \
printf '%s %s\\n' \"$$\" \"$pg\" > \"$pidfile\" 2>/dev/null || :; \
if [ -f \"$envfile\" ]; then while IFS= read -r l; do [ -n \"$l\" ] && set -- \"$l\" \"$@\"; done < \"$envfile\"; rm -f -- \"$envfile\"; fi; \
exec /usr/bin/env -i \"$@\"";

/// Script of [`RemoteCall::ReadContract`]: `$1` worktree, `$2` file name below `.fleet`.
const READ_CONTRACT: &str = "cd -- \"$1\" 2>/dev/null || exit 3; f=.fleet/$2; \
[ -f \"$f\" ] && [ ! -L \"$f\" ] || exit 3; head -c 65537 < \"$f\"";

/// Script of [`RemoteCall::Finalize`]: `$1` worktree, `$2` the commit that was sent. Commits what
/// the worker left (never `.fleet`) and writes a bundle of the new commits to stdout, nothing
/// when there is no change. Everything else git prints goes to stderr.
const FINALIZE: &str = "set -eu; cd -- \"$1\"; sent=$2; \
g add -A 1>&2; g reset -q -- .fleet 1>&2 || :; \
if ! g diff --cached --quiet --no-ext-diff; then g commit -q --no-verify --no-gpg-sign -m 'fleet: remote task changes' 1>&2; fi; \
new=$(g rev-parse --verify 'HEAD^{commit}'); \
if [ \"$new\" = \"$sent\" ]; then exit 0; fi; \
g update-ref refs/heads/fleet-return \"$new\"; \
g bundle create -q - refs/heads/fleet-return \"^$sent\"";

/// Script of [`RemoteCall::Signal`]: `$1` process id file, `$2` signal name. Signals the group
/// only while the recorded process still has the recorded group.
const SIGNAL: &str = "read -r pid pg < \"$1\" 2>/dev/null || exit 0; \
case $pid$pg in ''|*[!0-9]*) exit 1;; esac; \
[ \"$pid\" -gt 1 ] && [ \"$pg\" -gt 1 ] || exit 1; \
cur=$(ps -o pgid= -p \"$pid\" 2>/dev/null | tr -d ' '); \
[ \"$cur\" = \"$pg\" ] || exit 0; \
kill -s \"$2\" -- \"-$pg\" 2>/dev/null || kill -s \"$2\" \"$pid\" 2>/dev/null || :";

/// Script of [`RemoteCall::Remove`]: `$1` base, `$2` worktree, `$3` process id file. Refuses
/// anything that is not strictly below `<base>/<run>/`.
const REMOVE: &str = "base=$1; dir=$2; pidfile=$3; envfile=$4; \
case $dir in \"$base\"/?*/?*) ;; *) echo 'fleet: directory outside the base directory' >&2; exit 71;; esac; \
case $dir in */../*|*/./*|*/..|*/.) exit 71;; esac; \
case $pidfile in \"$base\"/?*/?*) ;; *) exit 71;; esac; \
case $envfile in \"$base\"/?*/?*) ;; *) exit 71;; esac; \
rm -rf -- \"$dir\" \"$pidfile\" \"$envfile\"";

/// Script of [`RemoteCall::CheckBase`]: `$1` base. Creates it when needed and says `ok` when it is
/// a plain, writable directory.
/// Script of [`RemoteCall::PutEnv`]: `$1` base, `$2` the environment file; the variables on stdin,
/// one `NAME=value` per line. The launcher reads the file and deletes it: values never appear on
/// a command line.
const PUT_ENV: &str = "set -eu; umask 077; base=$1; f=$2; case $f in \"$base\"/?*/?*) ;; *) echo 'fleet: file outside the base directory' >&2; exit 71;; esac; cat > \"$f\"";

const CHECK_BASE: &str = "umask 077; mkdir -p -- \"$1\" && [ -d \"$1\" ] && [ ! -L \"$1\" ] && [ -w \"$1\" ] && [ -O \"$1\" ] && case $(ls -ld -- \"$1\" | cut -c1-10) in ?????w????|????????w?) false;; *) true;; esac && echo ok";

/// Script of [`RemoteCall::RoundTrip`]: prints every argument followed by a NUL, then `uname -s`
/// and the user id the command runs as, one per line.
const ROUND_TRIP: &str = "printf '%s\\0' \"$@\"; uname -s; id -u";

/// Every constant script, for tests and for the documentation of what runs on a remote host.
pub const SCRIPTS: [(&str, &str); 10] = [
    ("prepare", PREPARE),
    ("put-file", PUT_FILE),
    ("put-env", PUT_ENV),
    ("launch", LAUNCH),
    ("read-contract", READ_CONTRACT),
    ("finalize", FINALIZE),
    ("signal", SIGNAL),
    ("remove", REMOVE),
    ("check-base", CHECK_BASE),
    ("round-trip", ROUND_TRIP),
];

// ---- calls ---------------------------------------------------------------------------------------------

/// A file fleet writes into the `.fleet` directory of a remote worktree before the worker starts:
/// what a local worker finds there as well (the brief, and the compiled context with its manifest
/// when the run uses compiled context).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputFile {
    Brief,
    Context,
    Manifest,
}

impl InputFile {
    pub fn file_name(self) -> &'static str {
        match self {
            InputFile::Brief => "brief.md",
            InputFile::Context => "context.md",
            InputFile::Manifest => "context.json",
        }
    }
}

/// A worker contract file that is read back from a remote worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractFile {
    Result,
    Question,
    /// The summary a worker leaves for the next attempt (compiled context).
    Handoff,
}

impl ContractFile {
    pub fn file_name(self) -> &'static str {
        match self {
            ContractFile::Result => "result.json",
            ContractFile::Question => "question.json",
            ContractFile::Handoff => "handoff.json",
        }
    }
}

/// A signal that may be sent to a remote worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteSignal {
    Term,
    Kill,
}

impl RemoteSignal {
    fn name(self) -> &'static str {
        match self {
            RemoteSignal::Term => "TERM",
            RemoteSignal::Kill => "KILL",
        }
    }
}

/// What may be run on a remote host. There is no variant that takes a command line: the closed
/// set below is the whitelist.
#[derive(Clone, Debug)]
pub enum RemoteCall<'a> {
    /// `git --version`.
    GitVersion,
    /// Prints the arguments back (NUL separated) and the system name: shows that this host's
    /// login shell reads quoted text the way it must.
    RoundTrip { args: &'a [String] },
    /// Creates the base directory when needed and checks it is writable.
    CheckBase { base: &'a str },
    /// Creates `<base>/<run>/<task>` as a git repository from the bundle on stdin and checks out
    /// branch `work` at the commit `fleet-sent`.
    Prepare {
        layout: &'a RemoteLayout,
        run: &'a RunId,
        task: &'a TaskId,
    },
    /// Writes stdin to a file of the `.fleet` directory of the worktree.
    PutFile {
        layout: &'a RemoteLayout,
        run: &'a RunId,
        task: &'a TaskId,
        file: InputFile,
    },
    /// Writes stdin (the text of [`env_file_text`]) to the environment file of the task.
    PutEnv {
        layout: &'a RemoteLayout,
        run: &'a RunId,
        task: &'a TaskId,
    },
    /// Starts the worker in the worktree with the environment of the file written by `PutEnv`
    /// (and nothing else). `program` must be one of `allowed`.
    Launch {
        layout: &'a RemoteLayout,
        run: &'a RunId,
        task: &'a TaskId,
        program: &'a str,
        allowed: &'a [String],
        args: &'a [String],
    },
    /// Reads one contract file (at most [`REMOTE_CONTRACT_READ`] bytes).
    ReadContract {
        layout: &'a RemoteLayout,
        run: &'a RunId,
        task: &'a TaskId,
        file: ContractFile,
    },
    /// Commits what the worker left and sends a bundle of the commits since `sent`.
    Finalize {
        layout: &'a RemoteLayout,
        run: &'a RunId,
        task: &'a TaskId,
        sent: &'a Sha,
    },
    /// Signals the worker's process group.
    Signal {
        layout: &'a RemoteLayout,
        run: &'a RunId,
        task: &'a TaskId,
        signal: RemoteSignal,
    },
    /// Removes the worktree and the process id file.
    Remove {
        layout: &'a RemoteLayout,
        run: &'a RunId,
        task: &'a TaskId,
    },
}

/// The fixed environment of every remote call that is not the worker: nothing from the local
/// machine, no user configuration of git.
fn fixed_env(path: &str, home: &str) -> Vec<(String, String)> {
    [
        ("PATH", path),
        ("HOME", home),
        ("LC_ALL", "C"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_NO_REPLACE_OBJECTS", "1"),
        ("GIT_PAGER", "cat"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn env_words(env: &[(String, String)]) -> Result<Vec<String>> {
    if env.len() > MAX_REMOTE_ENV {
        return Err(FleetError::limit(format!(
            "more than {MAX_REMOTE_ENV} remote environment variables"
        )));
    }
    let mut words = vec![quote(ENV)?, "-i".to_string()];
    for (k, v) in env {
        validate_remote_env(k, v)?;
        words.push(quote(&format!("{k}={v}"))?);
    }
    Ok(words)
}

fn script_call(
    env: &[(String, String)],
    script: &str,
    with_git: bool,
    args: &[&str],
) -> Result<String> {
    let mut words = env_words(env)?;
    words.push(quote(SH)?);
    words.push("-c".to_string());
    let text = if with_git {
        format!("{GIT_FN}{script}")
    } else {
        script.to_string()
    };
    words.push(quote(&text)?);
    words.push(quote(SCRIPT_NAME)?);
    for a in args {
        words.push(quote(a)?);
    }
    join(&words)
}

impl RemoteCall<'_> {
    /// The command line for the remote login shell. `path` is the remote `PATH` of git and the
    /// helper programs. Fails when an argument is not acceptable or the line is too long.
    pub fn assemble(&self, path: &str) -> Result<String> {
        match self {
            RemoteCall::GitVersion => {
                let mut words = env_words(&fixed_env(path, "/"))?;
                words.push("git".to_string());
                words.push("--version".to_string());
                join(&words)
            }
            RemoteCall::RoundTrip { args } => {
                if args.len() > MAX_REMOTE_ARGS {
                    return Err(FleetError::limit("too many arguments"));
                }
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                script_call(&fixed_env(path, "/"), ROUND_TRIP, false, &args)
            }
            RemoteCall::CheckBase { base } => {
                validate_remote_base(base)?;
                script_call(&fixed_env(path, base), CHECK_BASE, false, &[base])
            }
            RemoteCall::Prepare { layout, run, task } => {
                let dir = layout.worktree(run, task);
                script_call(
                    &fixed_env(path, layout.base()),
                    PREPARE,
                    true,
                    &[layout.base(), &dir],
                )
            }
            RemoteCall::PutFile {
                layout,
                run,
                task,
                file,
            } => {
                let dir = layout.worktree(run, task);
                script_call(
                    &fixed_env(path, layout.base()),
                    PUT_FILE,
                    false,
                    &[&dir, file.file_name()],
                )
            }
            RemoteCall::PutEnv { layout, run, task } => script_call(
                &fixed_env(path, layout.base()),
                PUT_ENV,
                false,
                &[layout.base(), &layout.envfile(run, task)],
            ),
            RemoteCall::Launch {
                layout,
                run,
                task,
                program,
                allowed,
                args,
            } => {
                validate_remote_program(program)?;
                if !allowed.iter().any(|a| a == program) {
                    return Err(FleetError::forbidden(format!(
                        "worker command {} is not allowed on this host",
                        show(program)
                    )));
                }
                if args.len() > MAX_REMOTE_ARGS {
                    return Err(FleetError::limit(format!(
                        "more than {MAX_REMOTE_ARGS} arguments for a remote worker"
                    )));
                }
                let dir = layout.worktree(run, task);
                let pidfile = layout.pidfile(run, task);
                let mut words = env_words(&[("PATH".to_string(), path.to_string())])?;
                words.push(quote(SH)?);
                words.push("-c".to_string());
                words.push(quote(LAUNCH)?);
                words.push(quote(SCRIPT_NAME)?);
                words.push(quote(&dir)?);
                words.push(quote(&pidfile)?);
                words.push(quote(&layout.envfile(run, task))?);
                words.push(quote(program)?);
                for a in *args {
                    words.push(quote(a)?);
                }
                join(&words)
            }
            RemoteCall::ReadContract {
                layout,
                run,
                task,
                file,
            } => {
                let dir = layout.worktree(run, task);
                script_call(
                    &fixed_env(path, layout.base()),
                    READ_CONTRACT,
                    false,
                    &[&dir, file.file_name()],
                )
            }
            RemoteCall::Finalize {
                layout,
                run,
                task,
                sent,
            } => {
                let dir = layout.worktree(run, task);
                script_call(
                    &fixed_env(path, layout.base()),
                    FINALIZE,
                    true,
                    &[&dir, sent.as_str()],
                )
            }
            RemoteCall::Signal {
                layout,
                run,
                task,
                signal,
            } => {
                let pidfile = layout.pidfile(run, task);
                script_call(
                    &fixed_env(path, layout.base()),
                    SIGNAL,
                    false,
                    &[&pidfile, signal.name()],
                )
            }
            RemoteCall::Remove { layout, run, task } => {
                let dir = layout.worktree(run, task);
                let pidfile = layout.pidfile(run, task);
                script_call(
                    &fixed_env(path, layout.base()),
                    REMOVE,
                    false,
                    &[layout.base(), &dir, &pidfile, &layout.envfile(run, task)],
                )
            }
        }
    }
}

/// The text of the environment file: one `NAME=value` per line, every entry validated (no NUL, no
/// line break in a value, bounded).
pub fn env_file_text(env: &[(String, String)]) -> Result<String> {
    if env.len() > MAX_REMOTE_ENV {
        return Err(FleetError::limit(format!(
            "more than {MAX_REMOTE_ENV} remote environment variables"
        )));
    }
    let mut out = String::new();
    for (k, v) in env {
        validate_remote_env(k, v)?;
        out.push_str(k);
        out.push('=');
        out.push_str(v);
        out.push('\n');
    }
    Ok(out)
}

/// The environment of a remote worker: only what the operator wrote for the host plus the
/// variables fleet sets itself (`FLEET_RUN`, `FLEET_TASK`, `FLEET_WORKTREE`) and the terminal
/// type. Nothing of the local environment is ever part of it. Later entries replace earlier ones.
pub fn worker_env(
    host_env: &[(String, String)],
    run: &RunId,
    task: &TaskId,
    worktree: &str,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: &str| {
        if let Some(slot) = out.iter_mut().find(|(n, _)| n == k) {
            slot.1 = v.to_string();
        } else {
            out.push((k.to_string(), v.to_string()));
        }
    };
    put("PATH", DEFAULT_REMOTE_PATH);
    put("TERM", REMOTE_TERM);
    for (k, v) in host_env {
        put(k, v);
    }
    put("FLEET_RUN", run.as_str());
    put("FLEET_TASK", task.as_str());
    put("FLEET_WORKTREE", worktree);
    out
}

// ---- tests ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// What a POSIX shell makes of a command line, restricted to what [`quote`] may produce: bare
    /// words of harmless characters and single-quoted segments, separated by single spaces. Any
    /// other character outside quotes is an error: the shell would interpret it.
    fn lex(line: &str) -> std::result::Result<Vec<String>, String> {
        let mut words = Vec::new();
        let mut cur = String::new();
        let mut has = false;
        let mut in_quote = false;
        let mut escaped = false;
        for c in line.chars() {
            if escaped {
                // The only escape `quote` writes outside quotes: `\'` (between two quoted parts).
                if c != '\'' {
                    return Err(format!("backslash before {c:?}"));
                }
                cur.push(c);
                has = true;
                escaped = false;
            } else if in_quote {
                if c == '\'' {
                    in_quote = false;
                } else {
                    cur.push(c);
                }
            } else if c == '\\' {
                escaped = true;
            } else if c == '\'' {
                in_quote = true;
                has = true;
            } else if c == ' ' {
                if has {
                    words.push(std::mem::take(&mut cur));
                    has = false;
                } else {
                    return Err("two separators".to_string());
                }
            } else if is_bare(c) {
                cur.push(c);
                has = true;
            } else {
                return Err(format!("unquoted {c:?}"));
            }
        }
        if in_quote {
            return Err("unterminated quote".to_string());
        }
        if has {
            words.push(cur);
        }
        Ok(words)
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() as usize) % n.max(1)
        }
    }

    const NASTY: &[&str] = &[
        "'",
        "\"",
        "\\",
        "$",
        "`",
        ";",
        "&",
        "|",
        "<",
        ">",
        "(",
        ")",
        "{",
        "}",
        "*",
        "?",
        "!",
        "#",
        "~",
        " ",
        "\n",
        "\t",
        "\r",
        "=",
        "%",
        "\u{1b}",
        "\u{7f}",
        "é",
        "日",
        "\u{202e}",
        "a",
        "b",
        "-",
        "/",
        "..",
        "$(id)",
        "`id`",
        "'; id; '",
        "\\'",
        "''",
        "\"'\"",
        "${IFS}",
        "\n; rm -rf /\n",
    ];

    fn random_word(rng: &mut Lcg) -> String {
        let n = rng.below(12);
        (0..n).map(|_| NASTY[rng.below(NASTY.len())]).collect()
    }

    fn id_run() -> RunId {
        RunId::parse("r1").unwrap()
    }
    fn id_task() -> TaskId {
        TaskId::parse("t1").unwrap()
    }
    fn layout() -> RemoteLayout {
        RemoteLayout::new("/srv/fleet").unwrap()
    }

    #[test]
    fn quote_leaves_harmless_words_and_wraps_everything_else() {
        assert_eq!(quote("abc-1_2.3/x:y@z,+%").unwrap(), "abc-1_2.3/x:y@z,+%");
        assert_eq!(quote("").unwrap(), "''");
        assert_eq!(quote("a b").unwrap(), "'a b'");
        assert_eq!(quote("it's").unwrap(), "'it'\\''s'");
        assert_eq!(quote("$(id)").unwrap(), "'$(id)'");
        assert_eq!(quote("~").unwrap(), "'~'");
        assert_eq!(quote("A=b").unwrap(), "'A=b'");
        assert_eq!(quote("a\nb").unwrap(), "'a\nb'");
        assert!(quote("a\0b").is_err());
    }

    #[test]
    fn property_a_quoted_word_is_read_back_unchanged_and_nothing_stays_unquoted() {
        let mut rng = Lcg(7);
        for _ in 0..5000 {
            let words: Vec<String> = (0..1 + rng.below(5))
                .map(|_| random_word(&mut rng))
                .collect();
            let line = words
                .iter()
                .map(|w| quote(w).unwrap())
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(lex(&line).unwrap(), words, "line: {line:?}");
        }
    }

    #[test]
    fn property_hostile_arguments_stay_single_words_and_values_never_reach_a_command_line() {
        let mut rng = Lcg(99);
        let l = layout();
        let (run, task) = (id_run(), id_task());
        let allowed = vec!["agent".to_string()];
        for _ in 0..2000 {
            let args: Vec<String> = (0..rng.below(6)).map(|_| random_word(&mut rng)).collect();
            let line = RemoteCall::Launch {
                layout: &l,
                run: &run,
                task: &task,
                program: "agent",
                allowed: &allowed,
                args: &args,
            }
            .assemble(DEFAULT_REMOTE_PATH)
            .unwrap();
            let words = lex(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"));
            // /usr/bin/env -i PATH=.. /bin/sh -c SCRIPT fleet-remote DIR PIDFILE ENVFILE PROGRAM ARGS...
            assert_eq!(words[0], ENV);
            assert_eq!(words[1], "-i");
            assert_eq!(words[2], format!("PATH={DEFAULT_REMOTE_PATH}"));
            assert_eq!(words[3], SH);
            assert_eq!(words[4], "-c");
            assert_eq!(words[5], LAUNCH);
            assert_eq!(words[6], SCRIPT_NAME);
            assert_eq!(words[7], "/srv/fleet/r1/t1");
            assert_eq!(words[8], "/srv/fleet/r1/t1.pid");
            assert_eq!(words[9], "/srv/fleet/r1/t1.env");
            assert_eq!(words[10], "agent");
            assert_eq!(&words[11..], &args[..]);

            let line = RemoteCall::RoundTrip { args: &args }
                .assemble(DEFAULT_REMOTE_PATH)
                .unwrap();
            let words = lex(&line).unwrap();
            assert_eq!(&words[words.len() - args.len()..], &args[..]);

            // Variables travel in a file, never in a command: every entry survives in its own line
            // or is refused.
            let env: Vec<(String, String)> = (0..rng.below(4))
                .map(|i| (format!("V{i}"), random_word(&mut rng)))
                .collect();
            match env_file_text(&env) {
                Ok(text) => {
                    let lines: Vec<&str> = text.split_terminator('\n').collect();
                    assert_eq!(lines.len(), env.len());
                    for (line, (k, v)) in lines.iter().zip(&env) {
                        assert_eq!(*line, format!("{k}={v}"));
                    }
                }
                Err(_) => assert!(env.iter().any(|(_, v)| v.contains('\n'))),
            }
        }
    }

    #[test]
    fn a_secret_value_is_in_no_command_line_only_in_the_environment_file() {
        let l = layout();
        let (run, task) = (id_run(), id_task());
        let allowed = vec!["agent".to_string()];
        let env = vec![("API_KEY".to_string(), "top-secret-value-1234".to_string())];
        let calls = [
            RemoteCall::Launch {
                layout: &l,
                run: &run,
                task: &task,
                program: "agent",
                allowed: &allowed,
                args: &[],
            },
            RemoteCall::PutEnv {
                layout: &l,
                run: &run,
                task: &task,
            },
            RemoteCall::Remove {
                layout: &l,
                run: &run,
                task: &task,
            },
        ];
        for call in calls {
            let line = call.assemble(DEFAULT_REMOTE_PATH).unwrap();
            assert!(!line.contains("top-secret"), "{line}");
        }
        assert_eq!(
            env_file_text(&env).unwrap(),
            "API_KEY=top-secret-value-1234\n"
        );
        assert!(env_file_text(&[("A".into(), "x\ny".into())]).is_err());
        assert!(env_file_text(&[("A B".into(), "x".into())]).is_err());
        assert!(env_file_text(&[("A".into(), "x\0".into())]).is_err());
        let many: Vec<(String, String)> = (0..=MAX_REMOTE_ENV)
            .map(|i| (format!("V{i}"), "1".into()))
            .collect();
        assert!(env_file_text(&many).is_err());
    }

    #[test]
    fn every_call_assembles_to_lexable_text_with_a_constant_script() {
        let l = layout();
        let (run, task) = (id_run(), id_task());
        let sent = Sha::parse(&"a".repeat(40)).unwrap();
        let calls = [
            RemoteCall::GitVersion,
            RemoteCall::CheckBase { base: "/srv/fleet" },
            RemoteCall::Prepare {
                layout: &l,
                run: &run,
                task: &task,
            },
            RemoteCall::PutFile {
                layout: &l,
                run: &run,
                task: &task,
                file: InputFile::Context,
            },
            RemoteCall::ReadContract {
                layout: &l,
                run: &run,
                task: &task,
                file: ContractFile::Result,
            },
            RemoteCall::Finalize {
                layout: &l,
                run: &run,
                task: &task,
                sent: &sent,
            },
            RemoteCall::Signal {
                layout: &l,
                run: &run,
                task: &task,
                signal: RemoteSignal::Kill,
            },
            RemoteCall::Remove {
                layout: &l,
                run: &run,
                task: &task,
            },
        ];
        for call in calls {
            let line = call.assemble("/usr/bin:/bin").unwrap();
            let words = lex(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"));
            assert_eq!(words[0], ENV);
            assert_eq!(words[1], "-i");
            let program = words
                .iter()
                .position(|w| w == SH || w == "git")
                .map(|i| words[i].clone())
                .unwrap();
            if program == SH {
                let at = words.iter().position(|w| w == "-c").unwrap();
                let script = &words[at + 1];
                assert!(
                    SCRIPTS
                        .iter()
                        .any(|(_, s)| script == s || script == &format!("{GIT_FN}{s}")),
                    "script not in the constant set"
                );
            }
        }
    }

    #[test]
    fn the_worker_command_must_be_on_the_hosts_list() {
        let l = layout();
        let (run, task) = (id_run(), id_task());
        let allowed = vec!["agent".to_string(), "/opt/bin/other".to_string()];
        let call = |program: &str| {
            RemoteCall::Launch {
                layout: &l,
                run: &run,
                task: &task,
                program,
                allowed: &allowed,
                args: &[],
            }
            .assemble(DEFAULT_REMOTE_PATH)
        };
        assert!(call("agent").is_ok());
        assert!(call("/opt/bin/other").is_ok());
        for refused in [
            "sh",
            "rm",
            "agent; id",
            "-agent",
            "",
            "../agent",
            "a b",
            "agent\n",
        ] {
            assert!(call(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn long_lines_and_too_many_arguments_are_refused() {
        let l = layout();
        let (run, task) = (id_run(), id_task());
        let allowed = vec!["agent".to_string()];
        let launch = |args: &[String]| {
            RemoteCall::Launch {
                layout: &l,
                run: &run,
                task: &task,
                program: "agent",
                allowed: &allowed,
                args,
            }
            .assemble(DEFAULT_REMOTE_PATH)
        };
        let big = vec!["x".repeat(MAX_REMOTE_COMMAND)];
        assert_eq!(
            launch(&big).unwrap_err().code,
            crate::error::ErrorCode::Limit
        );
        assert!(launch(&["a\0b".to_string()]).is_err());
        let args: Vec<String> = (0..=MAX_REMOTE_ARGS).map(|_| "a".to_string()).collect();
        assert!(launch(&args).is_err());
    }

    #[test]
    fn addresses_users_ports_and_paths_are_validated() {
        for ok in [
            "example.com",
            "build-1.internal",
            "10.0.0.7",
            "::1",
            "fe80::1:2",
            "2001:db8::7",
            "a",
        ] {
            assert!(validate_address(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            "a b",
            "a;b",
            "user@host",
            "host:22",
            "[::1]",
            "a..b",
            ".a",
            "a.",
            "a/b",
            "fe80::1%eth0",
            "-",
            "a_b",
            "host\n",
            "$(id)",
            &"a".repeat(254),
            ":::",
            "g::1",
        ] {
            assert!(validate_address(bad).is_err(), "{bad:?}");
        }
        for ok in ["alice", "_svc", "a.b-c_d", "A1"] {
            assert!(validate_user(ok).is_ok());
        }
        for bad in [
            "",
            "-o",
            "1abc",
            "a b",
            "a@b",
            "a:b",
            &"a".repeat(33),
            "a\n",
        ] {
            assert!(validate_user(bad).is_err(), "{bad:?}");
        }
        assert_eq!(validate_port(22).unwrap(), 22);
        assert!(validate_port(0).is_err());
        assert!(validate_port(65536).is_err());
        for ok in ["/home/a/.fleet-remote", "/srv/fleet", "/a"] {
            assert!(validate_remote_base(ok).is_ok(), "{ok}");
        }
        for bad in [
            "/",
            "srv",
            "",
            "~/x",
            "/a/../b",
            "/a/./b",
            "/a//b",
            "/a/b/",
            "/a b",
            "/a;b",
            "/a'b",
            "/a$b",
            "/a%b",
            "/a\nb",
            "/a/..",
            &format!("/{}", "a".repeat(300)),
        ] {
            assert!(validate_remote_base(bad).is_err(), "{bad:?}");
            assert!(
                validate_local_path("p", bad).is_err() || bad == "/",
                "{bad:?}"
            );
        }
        assert!(validate_local_path("identity file", "/home/me/.ssh/id_ed25519").is_ok());
    }

    #[test]
    fn remote_paths_stay_below_the_base_and_pid_files_beside_the_worktree() {
        let l = layout();
        assert_eq!(l.worktree(&id_run(), &id_task()), "/srv/fleet/r1/t1");
        assert_eq!(l.pidfile(&id_run(), &id_task()), "/srv/fleet/r1/t1.pid");
        assert!(RemoteLayout::new("/a/../b").is_err());
        // Ids cannot contain '/' or '.', so no id makes the path leave the base.
        for id in ["..", "a/b", "a.b", ".", ""] {
            assert!(RunId::parse(id).is_err() && TaskId::parse(id).is_err());
        }
    }

    #[test]
    fn the_worker_environment_holds_only_explicit_values_and_fleets_own() {
        let host = vec![
            ("PATH".to_string(), "/opt/x/bin:/usr/bin".to_string()),
            (
                "API_BASE".to_string(),
                "https://example.invalid".to_string(),
            ),
        ];
        let env = worker_env(&host, &id_run(), &id_task(), "/srv/fleet/r1/t1");
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("PATH"), Some("/opt/x/bin:/usr/bin"));
        assert_eq!(get("API_BASE"), Some("https://example.invalid"));
        assert_eq!(get("TERM"), Some(REMOTE_TERM));
        assert_eq!(get("FLEET_RUN"), Some("r1"));
        assert_eq!(get("FLEET_WORKTREE"), Some("/srv/fleet/r1/t1"));
        assert_eq!(env.len(), 6);
        let plain = worker_env(&[], &id_run(), &id_task(), "/w");
        assert_eq!(get_in(&plain, "PATH"), Some(DEFAULT_REMOTE_PATH));
        // A host entry cannot override what fleet sets.
        let sneaky = worker_env(
            &[("FLEET_RUN".into(), "other".into())],
            &id_run(),
            &id_task(),
            "/w",
        );
        assert_eq!(get_in(&sneaky, "FLEET_RUN"), Some("r1"));
    }

    fn get_in<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
        env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn scripts_are_one_constant_each_without_untrusted_substitution() {
        for (name, script) in SCRIPTS {
            assert!(!script.contains('\0'), "{name}");
            assert!(script.len() < 2048, "{name} is long");
        }
        // The launcher is the only script that execs positional parameters.
        let execs: Vec<&str> = SCRIPTS
            .iter()
            .filter(|(_, s)| s.contains("exec "))
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(execs, vec!["launch"]);
    }
}
