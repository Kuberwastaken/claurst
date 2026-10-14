//! Static scanner for shell command lines, used by permission matching (#445).
//!
//! A command line is split into simple commands at the control operators
//! `;`, `&&`, `||`, `|`, `|&`, `&` and newlines. Each simple command keeps its
//! words after quote removal, without redirections. Anything whose effect
//! cannot be read off the text is reported as [`Scan::Opaque`]: command and
//! process substitution, backticks, heredocs, subshells and groups, compound
//! keywords, and commands that run other commands (`eval`, `bash -c`,
//! `xargs`, `sudo`, ...). Permission rules never auto-allow an opaque command
//! by pattern; it needs an explicit approval.
//!
//! The scanner is deliberately conservative: when in doubt it reports
//! `Opaque`. It does not execute or expand anything.

/// Longest command line the scanner analyses; longer input is opaque.
const MAX_INPUT_LEN: usize = 64 * 1024;

/// Result of scanning a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scan {
    /// The simple commands, in source order. Empty for a blank line.
    Commands(Vec<SimpleCommand>),
    /// The command line cannot be analysed statically.
    Opaque(OpaqueReason),
}

/// One simple command of a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    /// Words after quote removal, redirections removed. Leading `NAME=value`
    /// assignments are kept, so a rule for `cargo test` does not cover
    /// `RUSTFLAGS=... cargo test`.
    pub words: Vec<String>,
    /// The command redirects output to a file (`>`, `>>`, `>|`, `&>`,
    /// `>&file`). Redirection to `/dev/null` and file-descriptor duplication
    /// (`2>&1`) do not count.
    pub writes_file: bool,
}

impl SimpleCommand {
    /// The words as a shell-quoted command line; scanning it gives back
    /// `words`.
    pub fn text(&self) -> String {
        self.words.iter().map(|w| quote_word(w)).collect::<Vec<_>>().join(" ")
    }

    /// The words with leading `NAME=value` assignments removed and the
    /// command name reduced to its file name (`/bin/rm` becomes `rm`). Deny
    /// rules are checked against these as well as `words`, so neither a
    /// prefix assignment nor a path steps around a deny rule.
    pub fn normalized_words(&self) -> Vec<String> {
        let mut words: Vec<String> =
            self.words.iter().skip_while(|w| is_assignment(w)).cloned().collect();
        if let Some(name) = words.first_mut() {
            if let Some((_, base)) = name.rsplit_once('/') {
                *name = base.to_string();
            }
        }
        words
    }
}

/// Quote `word` for a shell command line when it contains anything but
/// plain characters.
pub fn quote_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Why a command line is opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpaqueReason {
    /// `$( ... )` or backticks.
    CommandSubstitution,
    /// `<( ... )` or `>( ... )`.
    ProcessSubstitution,
    /// `<<` heredoc.
    Heredoc,
    /// `( ... )`, `{ ...; }`, or a compound keyword (`if`, `for`, ...).
    Compound,
    /// A command that runs another command (`eval`, `bash -c`, `xargs`,
    /// `sudo`, `find -exec`, a command name taken from a variable, ...).
    DynamicExecution,
    /// Unterminated quote or trailing backslash.
    Unterminated,
    /// A redirection without a target, or input longer than the scanner limit.
    Invalid,
}

impl OpaqueReason {
    /// Short human-readable description.
    pub fn describe(self) -> &'static str {
        match self {
            Self::CommandSubstitution => "command substitution",
            Self::ProcessSubstitution => "process substitution",
            Self::Heredoc => "heredoc",
            Self::Compound => "subshell or compound command",
            Self::DynamicExecution => "runs another command",
            Self::Unterminated => "unterminated quote",
            Self::Invalid => "could not be parsed",
        }
    }
}

/// Words that start a compound command or group.
const COMPOUND_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "select", "function", "{", "}", "!", "coproc",
];

/// Commands that execute their arguments (or input) as another command.
const DYNAMIC_COMMANDS: &[&str] = &[
    "eval", "exec", "source", ".", "xargs", "sudo", "doas", "su", "env", "command", "builtin",
    "nohup", "nice", "time", "timeout", "watch", "parallel", "export",
];

/// Characters through which the shell can turn one word into other words
/// (parameter, brace and glob expansion).
const EXPANSION_CHARS: [char; 6] = ['$', '{', '}', '*', '?', '['];

/// Shells whose `-c` flag runs a command string.
const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "mksh", "fish", "csh", "tcsh"];

/// Interpreters whose `-c` / `-e` style flags run code from the command line
/// (`python3.12` and other versioned pythons are matched separately).
const INTERPRETERS: &[&str] = &["node", "nodejs", "deno", "bun", "perl", "ruby", "php", "lua"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Redirect {
    /// `<` or `<<<`: the target is read, not written.
    Input,
    /// `>`, `>>`, `>|`, `&>`, `&>>`.
    Output,
    /// `>&` / `<&`: a descriptor number, `-`, or (for `>&`) a file.
    Duplicate,
}

struct Scanner {
    commands: Vec<SimpleCommand>,
    words: Vec<String>,
    word: String,
    in_word: bool,
    writes_file: bool,
    redirect: Option<Redirect>,
}

impl Scanner {
    fn finish_word(&mut self) {
        if !self.in_word {
            return;
        }
        let word = std::mem::take(&mut self.word);
        self.in_word = false;
        match self.redirect.take() {
            None => self.words.push(word),
            Some(Redirect::Input) => {}
            Some(Redirect::Output) => self.writes_file |= word != "/dev/null",
            Some(Redirect::Duplicate) => {
                let descriptor = word == "-" || word.chars().all(|c| c.is_ascii_digit());
                self.writes_file |= !descriptor && word != "/dev/null";
            }
        }
    }

    fn finish_command(&mut self) -> Result<(), OpaqueReason> {
        self.finish_word();
        if self.redirect.is_some() {
            return Err(OpaqueReason::Invalid);
        }
        if !self.words.is_empty() || self.writes_file {
            self.commands.push(SimpleCommand {
                words: std::mem::take(&mut self.words),
                writes_file: self.writes_file,
            });
        }
        self.writes_file = false;
        Ok(())
    }

    /// Start a redirection. A pending all-digit word is the descriptor
    /// number (`2>`), not an argument.
    fn start_redirect(&mut self, kind: Redirect) -> Result<(), OpaqueReason> {
        if self.in_word && !self.word.is_empty() && self.word.chars().all(|c| c.is_ascii_digit()) {
            self.word.clear();
            self.in_word = false;
        }
        self.finish_word();
        if self.redirect.is_some() {
            return Err(OpaqueReason::Invalid);
        }
        self.redirect = Some(kind);
        Ok(())
    }
}

/// Scan `command` into simple commands. See the module docs.
pub fn scan(command: &str) -> Scan {
    if command.len() > MAX_INPUT_LEN {
        return Scan::Opaque(OpaqueReason::Invalid);
    }
    match split(command) {
        Ok(commands) => match commands.iter().find_map(opaque_command) {
            Some(reason) => Scan::Opaque(reason),
            None => Scan::Commands(commands),
        },
        Err(reason) => Scan::Opaque(reason),
    }
}

fn split(command: &str) -> Result<Vec<SimpleCommand>, OpaqueReason> {
    let chars: Vec<char> = command.chars().collect();
    let mut s = Scanner {
        commands: Vec::new(),
        words: Vec::new(),
        word: String::new(),
        in_word: false,
        writes_file: false,
        redirect: None,
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\\' => match next {
                Some('\n') => i += 2,
                Some(escaped) => {
                    s.word.push(escaped);
                    s.in_word = true;
                    i += 2;
                }
                None => return Err(OpaqueReason::Unterminated),
            },
            '\'' => {
                let close = chars[i + 1..]
                    .iter()
                    .position(|&ch| ch == '\'')
                    .ok_or(OpaqueReason::Unterminated)?;
                s.word.extend(&chars[i + 1..i + 1 + close]);
                s.in_word = true;
                i += close + 2;
            }
            '"' => {
                i += 1;
                loop {
                    let ch = *chars.get(i).ok_or(OpaqueReason::Unterminated)?;
                    match ch {
                        '"' => break,
                        '`' => return Err(OpaqueReason::CommandSubstitution),
                        '$' if chars.get(i + 1) == Some(&'(') => {
                            return Err(OpaqueReason::CommandSubstitution)
                        }
                        '\\' => match chars.get(i + 1).copied() {
                            Some(escaped @ ('$' | '`' | '"' | '\\')) => {
                                s.word.push(escaped);
                                i += 1;
                            }
                            Some('\n') => i += 1,
                            _ => s.word.push('\\'),
                        },
                        other => s.word.push(other),
                    }
                    i += 1;
                }
                s.in_word = true;
                i += 1;
            }
            '`' => return Err(OpaqueReason::CommandSubstitution),
            '$' if next == Some('(') => return Err(OpaqueReason::CommandSubstitution),
            '#' if !s.in_word => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ' ' | '\t' | '\r' => {
                s.finish_word();
                i += 1;
            }
            '\n' | ';' => {
                s.finish_command()?;
                i += 1;
            }
            '&' => match next {
                Some('&') => {
                    s.finish_command()?;
                    i += 2;
                }
                Some('>') => {
                    s.start_redirect(Redirect::Output)?;
                    i += if chars.get(i + 2) == Some(&'>') { 3 } else { 2 };
                }
                _ => {
                    s.finish_command()?;
                    i += 1;
                }
            },
            '|' => {
                s.finish_command()?;
                i += if matches!(next, Some('|') | Some('&')) { 2 } else { 1 };
            }
            '(' | ')' => return Err(OpaqueReason::Compound),
            '<' => match next {
                Some('(') => return Err(OpaqueReason::ProcessSubstitution),
                Some('<') if chars.get(i + 2) == Some(&'<') => {
                    s.start_redirect(Redirect::Input)?;
                    i += 3;
                }
                Some('<') => return Err(OpaqueReason::Heredoc),
                Some('&') => {
                    s.start_redirect(Redirect::Duplicate)?;
                    i += 2;
                }
                Some('>') => {
                    s.start_redirect(Redirect::Output)?;
                    i += 2;
                }
                _ => {
                    s.start_redirect(Redirect::Input)?;
                    i += 1;
                }
            },
            '>' => match next {
                Some('(') => return Err(OpaqueReason::ProcessSubstitution),
                Some('&') => {
                    s.start_redirect(Redirect::Duplicate)?;
                    i += 2;
                }
                Some('>') | Some('|') => {
                    s.start_redirect(Redirect::Output)?;
                    i += 2;
                }
                _ => {
                    s.start_redirect(Redirect::Output)?;
                    i += 1;
                }
            },
            other => {
                s.word.push(other);
                s.in_word = true;
                i += 1;
            }
        }
    }
    s.finish_command()?;
    Ok(s.commands)
}

/// Whether `word` is a `NAME=value` assignment.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The reason a single simple command is opaque, if it is.
fn opaque_command(command: &SimpleCommand) -> Option<OpaqueReason> {
    let mut words = command.words.iter().skip_while(|w| is_assignment(w));
    let name = words.next()?;
    if COMPOUND_KEYWORDS.contains(&name.as_str()) {
        return Some(OpaqueReason::Compound);
    }
    // A command name the shell would expand (`$CMD`, `r{m,}`, `r?`) is not
    // the name written; `[` and `[[` are the test commands themselves.
    if name.contains(EXPANSION_CHARS) && name != "[" && name != "[[" {
        return Some(OpaqueReason::DynamicExecution);
    }
    let base = name.rsplit('/').next().unwrap_or(name);
    if DYNAMIC_COMMANDS.contains(&base) {
        return Some(OpaqueReason::DynamicExecution);
    }
    if SHELLS.contains(&base)
        && words.any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
    {
        return Some(OpaqueReason::DynamicExecution);
    }
    // Interpreters running code given on the command line.
    let interpreter = INTERPRETERS.contains(&base)
        || base.strip_prefix("python").is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'));
    if interpreter
        && command
            .words
            .iter()
            .any(|a| matches!(a.as_str(), "-c" | "-e" | "-E" | "-p" | "--eval" | "--print"))
    {
        return Some(OpaqueReason::DynamicExecution);
    }
    if base == "find"
        && command
            .words
            .iter()
            .any(|a| matches!(a.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
    {
        return Some(OpaqueReason::DynamicExecution);
    }
    None
}

/// How many leading words identify a command for a saved approval. Ported
/// from opencode's shell arity table (`packages/core/src/shell/parse.ts`):
/// `git status` is two words, `npm run build` three. Commands not listed use
/// one word.
const ARITY: &[(&str, usize)] = &[
    ("cat", 1), ("cd", 1), ("chmod", 1), ("chown", 1), ("cp", 1), ("echo", 1), ("env", 1),
    ("export", 1), ("grep", 1), ("kill", 1), ("killall", 1), ("ln", 1), ("ls", 1),
    ("mkdir", 1), ("mv", 1), ("ps", 1), ("pwd", 1), ("rm", 1), ("rmdir", 1), ("sleep", 1),
    ("source", 1), ("tail", 1), ("touch", 1), ("unset", 1), ("which", 1),
    ("aws", 3), ("az", 3), ("bazel", 2), ("brew", 2), ("bun", 2), ("bun run", 3), ("bun x", 3),
    ("cargo", 2), ("cargo add", 3), ("cargo run", 3), ("cdk", 2), ("cf", 2), ("cmake", 2),
    ("composer", 2), ("consul", 2), ("consul kv", 3), ("crictl", 2), ("deno", 2),
    ("deno task", 3), ("doctl", 3), ("docker", 2), ("docker builder", 3),
    ("docker compose", 3), ("docker container", 3), ("docker image", 3),
    ("docker network", 3), ("docker volume", 3), ("eksctl", 2), ("eksctl create", 3),
    ("firebase", 2), ("flyctl", 2), ("gcloud", 3), ("gh", 3), ("git", 2), ("git config", 3),
    ("git remote", 3), ("git stash", 3), ("go", 2), ("gradle", 2), ("helm", 2), ("heroku", 2),
    ("hugo", 2), ("ip", 2), ("ip addr", 3), ("ip link", 3), ("ip netns", 3), ("ip route", 3),
    ("kind", 2), ("kind create", 3), ("kubectl", 2), ("kubectl kustomize", 3),
    ("kubectl rollout", 3), ("kustomize", 2), ("make", 2), ("mc", 2), ("mc admin", 3),
    ("minikube", 2), ("mongosh", 2), ("mysql", 2), ("mvn", 2), ("ng", 2), ("npm", 2),
    ("npm exec", 3), ("npm init", 3), ("npm run", 3), ("npm view", 3), ("npx", 2), ("nvm", 2),
    ("nx", 2), ("openssl", 2), ("openssl req", 3), ("openssl x509", 3), ("pip", 2),
    ("pipenv", 2), ("pnpm", 2), ("pnpm dlx", 3), ("pnpm exec", 3), ("pnpm run", 3),
    ("poetry", 2), ("podman", 2), ("podman container", 3), ("podman image", 3), ("psql", 2),
    ("pulumi", 2), ("pulumi stack", 3), ("python", 2), ("pyenv", 2), ("rake", 2),
    ("rbenv", 2), ("redis-cli", 2), ("rustup", 2), ("serverless", 2), ("sfdx", 3),
    ("skaffold", 2), ("sls", 2), ("sst", 2), ("swift", 2), ("systemctl", 2), ("terraform", 2),
    ("terraform workspace", 3), ("tmux", 2), ("turbo", 2), ("ufw", 2), ("vault", 2),
    ("vault auth", 3), ("vault kv", 3), ("vercel", 2), ("volta", 2), ("wp", 2), ("yarn", 2),
    ("yarn dlx", 3), ("yarn run", 3),
];

/// The rule an "always allow" approval saves for `command`: the arity prefix
/// followed by ` *` (`git status *`, `npm run build *`). When the command is
/// shorter than its arity (a bare `git`), or starts with an environment
/// assignment, the rule is the exact command so the approval cannot widen to
/// every subcommand.
pub fn approval_rule(command: &SimpleCommand) -> String {
    let words = &command.words;
    if words.first().is_none_or(|w| is_assignment(w)) {
        return command.text();
    }
    let longest = words.len().min(3);
    let arity = (1..=longest)
        .rev()
        .find_map(|len| {
            let key = words[..len].join(" ");
            ARITY.iter().find(|(name, _)| *name == key).map(|(_, arity)| *arity)
        })
        .unwrap_or(1);
    if words.len() < arity {
        return command.text();
    }
    let prefix: Vec<String> = words[..arity].iter().map(|w| quote_word(w)).collect();
    format!("{} *", prefix.join(" "))
}

/// The distinct approval rules for every simple command of `command`, in
/// source order. Empty when the command is opaque or blank.
pub fn approval_rules(command: &str) -> Vec<String> {
    let Scan::Commands(commands) = scan(command) else {
        return Vec::new();
    };
    // A prefix rule never covers a command that writes to a file, so saving
    // one would not approve this line.
    if commands.iter().any(|c| c.writes_file) {
        return Vec::new();
    }
    let mut rules: Vec<String> = Vec::new();
    for rule in commands.iter().map(approval_rule) {
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    }
    rules
}

/// Whether the shell rule `pattern` covers `words`. A pattern is a command
/// line of one simple command; a final unquoted ` *` (or a lone `*`) matches
/// any, possibly empty, rest. Without it the words must match exactly.
/// Quoting in the pattern is honoured, so `git commit -m 'a b'` matches the
/// words `git`, `commit`, `-m`, `a b`.
pub fn pattern_matches(pattern: &str, words: &[String]) -> bool {
    pattern_matches_with(pattern, words, |token, word| token == word)
}

/// Whether the deny rule `pattern` may cover `words`. Like
/// [`pattern_matches`], except that a word the shell would expand (it holds
/// `$`, `{`, `}`, `*`, `?` or `[`) matches any token, so a variable or brace
/// expansion cannot step around a deny rule. Best effort: one word expanding
/// into several is not modelled.
pub fn pattern_denies(pattern: &str, words: &[String]) -> bool {
    pattern_matches_with(pattern, words, |token, word| {
        token == word || word.contains(EXPANSION_CHARS)
    })
}

fn pattern_matches_with(
    pattern: &str,
    words: &[String],
    eq: impl Fn(&str, &str) -> bool,
) -> bool {
    let pattern = pattern.trim();
    let (prefix, wildcard) = match pattern.strip_suffix('*') {
        Some(rest) if rest.is_empty() || rest.ends_with(char::is_whitespace) => (rest, true),
        _ => (pattern, false),
    };
    let tokens = match scan(prefix) {
        Scan::Commands(mut commands)
            if commands.len() <= 1 && commands.iter().all(|c| !c.writes_file) =>
        {
            commands.pop().map(|c| c.words).unwrap_or_default()
        }
        _ => return false,
    };
    if wildcard {
        words.len() >= tokens.len() && tokens.iter().zip(words).all(|(t, w)| eq(t.as_str(), w.as_str()))
    } else {
        !tokens.is_empty()
            && tokens.len() == words.len()
            && tokens.iter().zip(words).all(|(t, w)| eq(t.as_str(), w.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &str) -> Vec<Vec<String>> {
        match scan(command) {
            Scan::Commands(commands) => commands.into_iter().map(|c| c.words).collect(),
            Scan::Opaque(reason) => panic!("{command:?} is opaque: {reason:?}"),
        }
    }

    fn opaque(command: &str) -> OpaqueReason {
        match scan(command) {
            Scan::Opaque(reason) => reason,
            Scan::Commands(commands) => panic!("{command:?} scanned as {commands:?}"),
        }
    }

    fn w(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn splits_on_control_operators() {
        assert_eq!(
            words("git status && cargo test; ls -la | wc -l || echo fail\npwd & sleep 1 |& cat"),
            vec![
                w(&["git", "status"]),
                w(&["cargo", "test"]),
                w(&["ls", "-la"]),
                w(&["wc", "-l"]),
                w(&["echo", "fail"]),
                w(&["pwd"]),
                w(&["sleep", "1"]),
                w(&["cat"]),
            ]
        );
    }

    #[test]
    fn quotes_are_removed_and_protect_operators() {
        assert_eq!(
            words(r#"git commit -m "a && b; c" 'x | y' esc\ aped"#),
            vec![w(&["git", "commit", "-m", "a && b; c", "x | y", "esc aped"])]
        );
        assert_eq!(words(r#"echo "\$HOME \"q\"""#), vec![w(&["echo", "$HOME \"q\""])]);
    }

    #[test]
    fn blank_and_comment_lines_have_no_commands() {
        assert_eq!(words(""), Vec::<Vec<String>>::new());
        assert_eq!(words("  # just a comment\n"), Vec::<Vec<String>>::new());
        assert_eq!(words("ls # list\npwd"), vec![w(&["ls"]), w(&["pwd"])]);
        assert_eq!(words("echo a#b"), vec![w(&["echo", "a#b"])]);
    }

    #[test]
    fn line_continuation_joins_words() {
        assert_eq!(words("cargo \\\n  test"), vec![w(&["cargo", "test"])]);
    }

    #[test]
    fn redirections_are_dropped_and_writes_detected() {
        let Scan::Commands(c) = scan("ls > out.txt 2>&1") else { panic!() };
        assert_eq!(c[0].words, w(&["ls"]));
        assert!(c[0].writes_file);

        let Scan::Commands(c) = scan("cargo build 2>/dev/null >&2 < input.txt") else { panic!() };
        assert_eq!(c[0].words, w(&["cargo", "build"]));
        assert!(!c[0].writes_file);

        for cmd in ["echo x >> log", "echo x &> log", "echo x >| log", "echo x >&log", "echo 2>err"] {
            let Scan::Commands(c) = scan(cmd) else { panic!("{cmd}") };
            assert!(c[0].writes_file, "{cmd}");
        }

        let Scan::Commands(c) = scan("grep x <<< \"$var\"") else { panic!() };
        assert_eq!(c[0].words, w(&["grep", "x"]));
        assert!(!c[0].writes_file);
    }

    #[test]
    fn substitutions_are_opaque() {
        assert_eq!(opaque("echo $(rm -rf ~)"), OpaqueReason::CommandSubstitution);
        assert_eq!(opaque("echo \"$(whoami)\""), OpaqueReason::CommandSubstitution);
        assert_eq!(opaque("echo `id`"), OpaqueReason::CommandSubstitution);
        assert_eq!(opaque("echo \"`id`\""), OpaqueReason::CommandSubstitution);
        assert_eq!(opaque("echo $((1 + 2))"), OpaqueReason::CommandSubstitution);
        assert_eq!(opaque("diff <(ls a) <(ls b)"), OpaqueReason::ProcessSubstitution);
        assert_eq!(opaque("tee >(cat)"), OpaqueReason::ProcessSubstitution);
        // Single quotes keep `$(` literal.
        assert_eq!(words("echo '$(id)'"), vec![w(&["echo", "$(id)"])]);
    }

    #[test]
    fn heredocs_are_opaque() {
        assert_eq!(opaque("cat <<EOF\nrm -rf /\nEOF"), OpaqueReason::Heredoc);
        assert_eq!(opaque("cat <<-'EOF'\nx\nEOF"), OpaqueReason::Heredoc);
    }

    #[test]
    fn dynamic_execution_is_opaque() {
        for cmd in [
            "eval \"rm -rf /\"",
            "bash -c 'rm -rf /'",
            "sh -c ls",
            "/bin/bash -lc ls",
            "zsh -ec ls",
            "find . -name x | xargs rm",
            "sudo ls",
            "env FOO=1 rm x",
            "exec rm x",
            "source ./script.sh",
            ". ./script.sh",
            "nohup rm x",
            "time cargo build",
            "timeout 5 rm x",
            "find . -exec rm {} \\;",
            "$CMD arg",
            "FOO=1 $CMD",
            "git status; sudo rm -rf /",
        ] {
            assert_eq!(opaque(cmd), OpaqueReason::DynamicExecution, "{cmd}");
        }
        // Expanded names, `export`, and interpreters running inline code.
        for cmd in [
            "{rm,-rf,/tmp/x}",
            "r{m,} -rf x",
            "r? x",
            "export X=push",
            "python3 -c 'import os'",
            "python3.12 -c x",
            "node -e 'x'",
            "perl -e 'x'",
        ] {
            assert_eq!(opaque(cmd), OpaqueReason::DynamicExecution, "{cmd}");
        }
        assert_eq!(words("[ -f x ] && cat x"), vec![w(&["[", "-f", "x", "]"]), w(&["cat", "x"])]);
        assert_eq!(words("python3 script.py"), vec![w(&["python3", "script.py"])]);
        // A shell running a script file is an ordinary command.
        assert_eq!(words("bash ./build.sh --release"), vec![w(&["bash", "./build.sh", "--release"])]);
    }

    #[test]
    fn compound_commands_are_opaque() {
        for cmd in [
            "(cd /tmp && rm -rf x)",
            "{ ls; }",
            "if true; then rm x; fi",
            "for f in *; do rm $f; done",
            "while true; do :; done",
            "f() { rm x; }",
            "! grep x y",
        ] {
            assert_eq!(opaque(cmd), OpaqueReason::Compound, "{cmd}");
        }
    }

    #[test]
    fn malformed_input_is_opaque() {
        assert_eq!(opaque("echo 'unterminated"), OpaqueReason::Unterminated);
        assert_eq!(opaque("echo \"unterminated"), OpaqueReason::Unterminated);
        assert_eq!(opaque("echo trailing\\"), OpaqueReason::Unterminated);
        assert_eq!(opaque("echo >"), OpaqueReason::Invalid);
        assert_eq!(opaque("echo > ; ls"), OpaqueReason::Invalid);
        assert_eq!(opaque(&"a".repeat(MAX_INPUT_LEN + 1)), OpaqueReason::Invalid);
    }

    #[test]
    fn assignments_are_kept_as_words() {
        assert_eq!(
            words("RUST_LOG=debug cargo test"),
            vec![w(&["RUST_LOG=debug", "cargo", "test"])]
        );
    }

    #[test]
    fn approval_rules_use_arity() {
        assert_eq!(approval_rules("git status --short"), vec!["git status *"]);
        assert_eq!(approval_rules("npm run build -- --watch"), vec!["npm run build *"]);
        assert_eq!(approval_rules("ls -la"), vec!["ls *"]);
        assert_eq!(approval_rules("./build.sh release"), vec!["./build.sh *"]);
        assert_eq!(
            approval_rules("cargo test && git diff HEAD | head -5 && cargo test -p x"),
            vec!["cargo test *", "git diff *", "head *"]
        );
        // Shorter than the arity: exact command, never `git *`.
        assert_eq!(approval_rules("git"), vec!["git"]);
        assert_eq!(approval_rules("npm run"), vec!["npm run"]);
        // Leading assignment: exact command.
        assert_eq!(approval_rules("FOO=1 cargo test"), vec!["FOO=1 cargo test"]);
        // Opaque or blank: nothing to save.
        assert!(approval_rules("echo $(id)").is_empty());
        assert!(approval_rules("").is_empty());
    }

    #[test]
    fn pattern_matching() {
        assert!(pattern_matches("git status *", &w(&["git", "status"])));
        assert!(pattern_matches("git status *", &w(&["git", "status", "--short"])));
        assert!(!pattern_matches("git status *", &w(&["git", "push"])));
        assert!(!pattern_matches("git status *", &w(&["git"])));
        assert!(pattern_matches("git status", &w(&["git", "status"])));
        assert!(!pattern_matches("git status", &w(&["git", "status", "-s"])));
        assert!(pattern_matches("*", &w(&["anything", "at", "all"])));
        assert!(pattern_matches("npm run *", &w(&["npm", "run", "build"])));
        assert!(!pattern_matches("npm run *", &w(&["npm", "install"])));
        // Quoting in the pattern is honoured; a quoted `*` is literal.
        assert!(pattern_matches("git commit -m 'a b'", &w(&["git", "commit", "-m", "a b"])));
        assert!(pattern_matches("rm '*'", &w(&["rm", "*"])));
        assert!(!pattern_matches("rm '*'", &w(&["rm", "x"])));
        // Patterns that are not one plain simple command never match.
        assert!(!pattern_matches("", &w(&["ls"])));
        assert!(!pattern_matches("ls; rm *", &w(&["ls"])));
        assert!(!pattern_matches("echo $(id)", &w(&["echo", "$(id)"])));
        assert!(!pattern_matches("git*", &w(&["git", "push"])));
    }

    #[test]
    fn deny_patterns_treat_expanding_words_as_wildcards() {
        assert!(pattern_denies("git push *", &w(&["git", "{push,-f}"])));
        assert!(pattern_denies("git push *", &w(&["git", "$X", "-f"])));
        assert!(pattern_denies("git push *", &w(&["git", "pu?h"])));
        assert!(!pattern_denies("git push *", &w(&["git", "status"])));
        // Allow matching stays literal.
        assert!(!pattern_matches("git push *", &w(&["git", "$X"])));
    }

    #[test]
    fn redirecting_lines_have_no_approval_rules() {
        assert!(approval_rules("cargo build > build.log 2>&1").is_empty());
        assert_eq!(approval_rules("cargo build 2>&1 > /dev/null"), vec!["cargo build *"]);
    }

    #[test]
    fn text_round_trips_through_the_scanner() {
        for cmd in ["git commit -m 'fix the bug'", r#"echo "it's" '*' a=b"#, "ls -la /tmp"] {
            let Scan::Commands(c) = scan(cmd) else { panic!("{cmd}") };
            let Scan::Commands(again) = scan(&c[0].text()) else { panic!("{cmd}") };
            assert_eq!(again[0].words, c[0].words, "{cmd}");
        }
    }

    #[test]
    fn normalized_words_strip_assignments_and_paths() {
        let Scan::Commands(c) = scan("FOO=1 BAR=2 /usr/bin/rm -rf build") else { panic!() };
        assert_eq!(c[0].normalized_words(), w(&["rm", "-rf", "build"]));
    }
}
