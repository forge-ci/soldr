//! Best-effort literal argv extraction. Selection semantics live in the front
//! door, not here. Computed Python argv and shell expansions stay runtime work.

#[derive(Debug)]
struct Word {
    value: String,
    quoted: bool,
}

/// Shell commands, literal command strings in script/config wrappers, and
/// literal Python argv lists. Never interpret echo/print prose as commands.
pub(super) fn invocations(text: &str) -> Vec<Vec<String>> {
    let words = words(text);
    if words.first().is_some_and(|word| word.value == "print") {
        return Vec::new();
    }
    let wrapper = words
        .first()
        .is_some_and(|word| matches!(word.value.as_str(), "uv" | "env" | "sudo" | "command"));
    let mut commands = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let previous = index.checked_sub(1).map(|i| words[i].value.as_str());
        let starts = previous.is_none_or(|s| matches!(s, ";" | "&" | "|" | "(" | "[" | "=" | ":"))
            || previous
                .is_some_and(|s| s.contains('=') || matches!(s, "env" | "exec" | "uv" | "run"));
        if previous.is_some_and(|s| matches!(s, "echo" | "printf")) {
            continue;
        }
        if !starts && !wrapper {
            continue;
        }
        if word.quoted && starts && word.value != "soldr" {
            commands.extend(invocations(&word.value));
        } else if word.value == "soldr" {
            let mut args = Vec::new();
            let mut dynamic = false;
            let list = word.quoted && matches!(previous, Some("[" | "("));
            if list && index >= 2 && words[index - 2].value == "+" {
                continue;
            }
            for (offset, next) in words[index + 1..].iter().enumerate() {
                if list && matches!(next.value.as_str(), "]" | ")") {
                    dynamic |= words
                        .get(index + offset + 2)
                        .is_some_and(|word| word.value == "+");
                }
                if list
                    && next.value != ","
                    && !matches!(next.value.as_str(), "]" | ")")
                    && !next.quoted
                {
                    dynamic = true;
                }
                if matches!(next.value.as_str(), ";" | "&" | "|" | ")" | "]") {
                    break;
                }
                if next.value != "," {
                    args.push(next.value.clone());
                }
            }
            if !dynamic {
                commands.push(args);
            }
        }
    }
    commands
}

fn words(text: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let mut chars = text.chars().peekable();
    let mut value = String::new();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        if ch == '\'' || ch == '"' {
            quoted = true;
            while let Some(inner) = chars.next() {
                if inner == ch {
                    break;
                }
                if inner == '\\' && ch == '"' {
                    if let Some(escaped) = chars.next() {
                        value.push(escaped);
                    }
                } else {
                    value.push(inner);
                }
            }
        } else if ch.is_whitespace()
            || ";&|()[]".contains(ch)
            || (ch == ',' && (quoted || value.is_empty()))
            || (ch == '='
                && (value.is_empty() || matches!(value.as_str(), "command" | "cmd" | "run")))
            || (ch == ':' && value == "run")
        {
            if !value.is_empty() {
                words.push(Word {
                    value: std::mem::take(&mut value),
                    quoted,
                });
            }
            quoted = false;
            if !ch.is_whitespace() {
                words.push(Word {
                    value: ch.to_string(),
                    quoted: false,
                });
            }
        } else {
            value.push(ch);
        }
    }
    if !value.is_empty() {
        words.push(Word { value, quoted });
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_literals_find_commands_inside_config_wrappers() {
        assert_eq!(
            invocations("cmd = \"cd /repo && soldr cargo test -p a\""),
            vec![vec!["cargo", "test", "-p", "a"]]
        );
    }

    #[test]
    fn canonical_literals_preserve_quoted_values_and_ignore_prose() {
        for text in [
            "soldr cargo test --features 'a,b'",
            "env MODE=ci soldr cargo test --features 'a,b'",
            "uv run --no-project soldr cargo test --features 'a,b'",
            "subprocess.run(\"soldr cargo test --features 'a,b'\", shell=True)",
            "run(['soldr', 'cargo', 'test', '--features', 'a,b'])",
            "command = \"soldr cargo test --features 'a,b'\"",
        ] {
            assert_eq!(
                invocations(text),
                vec![vec!["cargo", "test", "--features", "a,b"]],
                "{text}"
            );
        }
        for text in [
            "echo 'soldr cargo test -p a'",
            "uv run echo 'soldr cargo test -p a'",
            "print('soldr cargo test -p a')",
            "print ('soldr cargo test -p a')",
        ] {
            assert!(invocations(text).is_empty(), "{text}");
        }
        for text in [
            "run(['soldr', 'cargo', 'test', *scope])",
            "run(['soldr', 'cargo', 'test'] + scope)",
            "run(scope + ['soldr', 'cargo', 'test'])",
        ] {
            assert!(invocations(text).is_empty(), "{text}");
        }
    }
}
