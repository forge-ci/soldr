//! Bounded declarative command parsing. Applications own their command names,
//! field mapping, and effect policy; Clap is a private implementation detail.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;

/// Maximum number of command-line words accepted by [`Command::parse`].
pub const MAX_ARGUMENTS: usize = 1_024;
/// Maximum UTF-8 bytes accepted for one command-line word.
pub const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
/// Maximum total UTF-8 bytes accepted for all command-line words.
pub const MAX_TOTAL_ARGUMENT_BYTES: usize = 1024 * 1024;

/// A facade-owned scalar validation rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValueKind {
    /// Any non-NUL UTF-8 string within the input limits.
    String,
    /// An operating-system string, retained without UTF-8 conversion.
    OsString,
    /// One of the declared strings.
    Enumeration(Vec<String>),
    /// A finite IEEE-754 double accepted by Rust's `f64` parser.
    F64,
    /// A non-negative 32-bit integer.
    U32,
}

impl ValueKind {
    /// Accept an arbitrary string value.
    pub const fn string() -> Self {
        Self::String
    }

    /// Accept a non-NUL operating-system string within the input limits.
    pub const fn os_string() -> Self {
        Self::OsString
    }

    /// Accept exactly one of `values`.
    pub fn enumeration<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Enumeration(values.into_iter().map(Into::into).collect())
    }

    /// Accept a Rust `f64` value.
    pub const fn f64() -> Self {
        Self::F64
    }

    /// Accept a Rust `u32` value.
    pub const fn u32() -> Self {
        Self::U32
    }
}

/// One long option in a [`Command`] schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OptionSpec {
    name: String,
    kind: Option<ValueKind>,
    default: Option<String>,
    default_missing: Option<String>,
    repeated: bool,
    conflicts: Vec<String>,
    requires_any: Vec<String>,
    help: Option<String>,
    hidden: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PositionalSpec {
    name: String,
    kind: ValueKind,
    optional: bool,
}

impl OptionSpec {
    /// Declare a boolean `--name` flag.
    pub fn flag(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: None,
            default: None,
            default_missing: None,
            repeated: false,
            conflicts: Vec::new(),
            requires_any: Vec::new(),
            help: None,
            hidden: false,
        }
    }

    /// Declare a string-valued `--name VALUE` option.
    pub fn value(name: impl Into<String>, kind: ValueKind) -> Self {
        Self {
            name: name.into(),
            kind: Some(kind),
            default: None,
            default_missing: None,
            repeated: false,
            conflicts: Vec::new(),
            requires_any: Vec::new(),
            help: None,
            hidden: false,
        }
    }

    /// Supply a value used when this option is absent.
    pub fn default(mut self, value: impl Into<String>) -> Self {
        self.default = Some(value.into());
        self
    }

    /// Allow this value option without a value and use `value` in that case.
    pub fn optional_value(mut self, value: impl Into<String>) -> Self {
        self.default_missing = Some(value.into());
        self
    }

    /// Preserve every occurrence of this value option in declaration order.
    pub fn repeated(mut self) -> Self {
        self.repeated = true;
        self
    }

    /// Reject this option when `other` is also present.
    pub fn conflicts(mut self, other: impl Into<String>) -> Self {
        self.conflicts.push(other.into());
        self
    }

    /// Require one of these option names whenever this option is present.
    pub fn requires_any<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.requires_any.extend(names.into_iter().map(Into::into));
        self
    }

    /// Describe this option in facade-rendered help.
    pub fn help(mut self, text: impl Into<String>) -> Self {
        self.help = Some(text.into());
        self
    }

    /// Keep this option parseable but omit it from facade-rendered help.
    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }
}

/// A declarative command and its nested subcommands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Command {
    name: String,
    about: Option<String>,
    version: Option<String>,
    options: Vec<OptionSpec>,
    positionals: Vec<PositionalSpec>,
    subcommands: Vec<Self>,
    exclusive_groups: Vec<(String, Vec<String>)>,
}

impl Command {
    /// Start a command schema.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            about: None,
            version: None,
            options: Vec::new(),
            positionals: Vec::new(),
            subcommands: Vec::new(),
            exclusive_groups: Vec::new(),
        }
    }

    /// Add a long option.
    pub fn option(mut self, option: OptionSpec) -> Self {
        self.options.push(option);
        self
    }

    /// Describe this command in facade-rendered help.
    pub fn about(mut self, text: impl Into<String>) -> Self {
        self.about = Some(text.into());
        self
    }

    /// Set the version rendered by [`Command::render_version`].
    pub fn version(mut self, value: impl Into<String>) -> Self {
        self.version = Some(value.into());
        self
    }

    /// Render deterministic, backend-independent version text.
    pub fn render_version(&self) -> String {
        match &self.version {
            Some(version) => format!("{} {version}\n", self.name),
            None => format!("{}\n", self.name),
        }
    }

    /// Render deterministic, backend-independent help for this command.
    pub fn render_help(&self) -> String {
        let mut output = format!("Usage: {}", self.name);
        if !self.options.is_empty() {
            output.push_str(" [OPTIONS]");
        }
        if !self.positionals.is_empty() {
            for positional in &self.positionals {
                let token = format!("<{}>", positional.name);
                if positional.optional {
                    let _ = write!(output, " [{token}]");
                } else {
                    let _ = write!(output, " {token}");
                }
            }
        }
        if !self.subcommands.is_empty() {
            output.push_str(" [COMMAND]");
        }
        output.push('\n');
        if let Some(about) = &self.about {
            let _ = write!(output, "\n{about}\n");
        }
        if !self.options.is_empty() {
            output.push_str("\nOptions:\n");
            for option in &self.options {
                if option.hidden {
                    continue;
                }
                let suffix = match &option.kind {
                    None => String::new(),
                    Some(_) if option.default_missing.is_some() => " [VALUE]".to_owned(),
                    Some(_) => " <VALUE>".to_owned(),
                };
                let repeat = if option.repeated { "..." } else { "" };
                let help = option.help.as_deref().unwrap_or("");
                let _ = writeln!(output, "  --{}{suffix}{repeat}\t{help}", option.name);
            }
        }
        if !self.subcommands.is_empty() {
            output.push_str("\nCommands:\n");
            for command in &self.subcommands {
                let about = command.about.as_deref().unwrap_or("");
                let _ = writeln!(output, "  {}\t{about}", command.name);
            }
        }
        output
    }

    /// Add a required positional value in declaration order.
    pub fn positional(mut self, name: impl Into<String>, kind: ValueKind) -> Self {
        self.positionals.push(PositionalSpec {
            name: name.into(),
            kind,
            optional: false,
        });
        self
    }

    /// Add an optional positional value in declaration order.
    pub fn optional_positional(mut self, name: impl Into<String>, kind: ValueKind) -> Self {
        self.positionals.push(PositionalSpec {
            name: name.into(),
            kind,
            optional: true,
        });
        self
    }

    /// Add a nested subcommand.
    pub fn subcommand(mut self, command: Self) -> Self {
        self.subcommands.push(command);
        self
    }

    /// Require at most one named option from this group.
    pub fn exclusive_group<I, S>(mut self, name: impl Into<String>, options: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exclusive_groups
            .push((name.into(), options.into_iter().map(Into::into).collect()));
        self
    }

    /// Parse one bounded command line into facade-owned values.
    ///
    /// The private parser never runs a shell. Input limits are checked before
    /// it receives any owned words; errors deliberately do not echo input.
    pub fn parse<I, S>(&self, arguments: I) -> Result<ParsedCommand, CommandError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut total = 0usize;
        let mut words = Vec::new();
        for argument in arguments {
            if words.len() == MAX_ARGUMENTS {
                return Err(CommandError::TooManyArguments);
            }
            let value = argument.as_ref();
            let bytes = value.as_encoded_bytes();
            if bytes.contains(&b'\0') {
                return Err(CommandError::ContainsNul);
            }
            if bytes.len() > MAX_ARGUMENT_BYTES {
                return Err(CommandError::ArgumentTooLarge);
            }
            total = total
                .checked_add(bytes.len())
                .ok_or(CommandError::InputTooLarge)?;
            if total > MAX_TOTAL_ARGUMENT_BYTES {
                return Err(CommandError::InputTooLarge);
            }
            words.push(value.to_os_string());
        }
        let explicit_options = explicit_option_names(&words)?;
        self.validate()?;
        let command = self.clap_command();
        let matches = command
            .try_get_matches_from(words)
            .map_err(|_| CommandError::InvalidArguments)?;
        let mut path = vec![self.name.clone()];
        let mut schema = self;
        let mut selected = vec![(self, &matches)];
        let mut final_matches = &matches;
        while let Some((name, next)) = final_matches.subcommand() {
            let Some(next_schema) = schema.subcommands.iter().find(|child| child.name == name)
            else {
                return Err(CommandError::InvalidArguments);
            };
            path.push(name.to_owned());
            schema = next_schema;
            final_matches = next;
            selected.push((schema, final_matches));
        }
        let mut values = BTreeMap::new();
        for (command, command_matches) in &selected {
            command.collect_values(command_matches, &mut values)?;
        }
        let selected_schemas = selected
            .iter()
            .map(|(command, _)| *command)
            .collect::<Vec<_>>();
        Self::validate_selected_relations(&selected_schemas, &explicit_options)?;
        Ok(ParsedCommand { path, values })
    }

    fn clap_command(&self) -> clap::Command {
        let mut command = clap::Command::new(self.name.clone())
            .disable_help_flag(false)
            .disable_version_flag(true)
            .disable_help_subcommand(true)
            .subcommand_precedence_over_arg(true);
        for option in &self.options {
            let mut argument = clap::Arg::new(option.name.clone())
                .long(option.name.clone())
                .global(true);
            match &option.kind {
                None => argument = argument.action(clap::ArgAction::SetTrue),
                Some(ValueKind::String) => argument = argument.action(clap::ArgAction::Set),
                Some(ValueKind::OsString) => {
                    argument = argument
                        .action(clap::ArgAction::Set)
                        .value_parser(clap::value_parser!(std::ffi::OsString));
                }
                Some(ValueKind::Enumeration(values)) => {
                    argument = argument
                        .action(clap::ArgAction::Set)
                        .value_parser(values.clone());
                }
                Some(ValueKind::F64) => {
                    argument = argument
                        .action(clap::ArgAction::Set)
                        .value_parser(clap::value_parser!(f64));
                }
                Some(ValueKind::U32) => {
                    argument = argument
                        .action(clap::ArgAction::Set)
                        .value_parser(clap::value_parser!(u32));
                }
            }
            if let Some(default) = &option.default {
                argument = argument.default_value(default);
            }
            if let Some(default_missing) = &option.default_missing {
                argument = argument
                    .num_args(0..=1)
                    .default_missing_value(default_missing);
            }
            if option.repeated {
                argument = argument.action(clap::ArgAction::Append);
            }
            command = command.arg(argument);
        }
        for (index, positional) in self.positionals.iter().enumerate() {
            let mut argument = clap::Arg::new(positional.name.clone())
                .index(index + 1)
                .required(!positional.optional)
                .action(clap::ArgAction::Set);
            match &positional.kind {
                ValueKind::String => {}
                ValueKind::OsString => {
                    argument = argument.value_parser(clap::value_parser!(std::ffi::OsString));
                }
                ValueKind::Enumeration(values) => argument = argument.value_parser(values.clone()),
                ValueKind::F64 => argument = argument.value_parser(clap::value_parser!(f64)),
                ValueKind::U32 => argument = argument.value_parser(clap::value_parser!(u32)),
            }
            command = command.arg(argument);
        }
        for child in &self.subcommands {
            command = command.subcommand(child.clap_command());
        }
        command
    }

    fn validate(&self) -> Result<(), CommandError> {
        let mut option_names = std::collections::BTreeSet::new();
        self.validate_into(&mut option_names)?;
        self.validate_references(&option_names)
    }

    fn validate_into(
        &self,
        option_names: &mut std::collections::BTreeSet<String>,
    ) -> Result<(), CommandError> {
        if !valid_name(&self.name) || self.name == "help" {
            return Err(CommandError::InvalidSchema);
        }
        let mut child_names = std::collections::BTreeSet::new();
        for option in &self.options {
            if !valid_name(&option.name)
                || option.name == "help"
                || !option_names.insert(option.name.clone())
            {
                return Err(CommandError::InvalidSchema);
            }
            match (&option.kind, &option.default, &option.default_missing) {
                (None, Some(_), _) | (None, _, Some(_)) => return Err(CommandError::InvalidSchema),
                (None, _, _) if option.repeated => return Err(CommandError::InvalidSchema),
                (Some(_), Some(_), Some(_)) | (Some(_), Some(_), _) if option.repeated => {
                    return Err(CommandError::InvalidSchema)
                }
                (Some(ValueKind::Enumeration(values)), default, default_missing)
                    if values.is_empty()
                        || values.iter().any(|value| value.contains('\0'))
                        || default
                            .as_ref()
                            .is_some_and(|value| !values.contains(value))
                        || default_missing
                            .as_ref()
                            .is_some_and(|value| !values.contains(value)) =>
                {
                    return Err(CommandError::InvalidSchema);
                }
                (_, Some(value), _) if value.contains('\0') => {
                    return Err(CommandError::InvalidSchema)
                }
                (_, _, Some(value)) if value.contains('\0') => {
                    return Err(CommandError::InvalidSchema)
                }
                _ => {}
            }
            if let Some(kind) = &option.kind {
                if (option.repeated && !matches!(kind, ValueKind::String | ValueKind::OsString))
                    || option
                        .default
                        .as_ref()
                        .is_some_and(|value| !valid_value(kind, value))
                    || option
                        .default_missing
                        .as_ref()
                        .is_some_and(|value| !valid_value(kind, value))
                {
                    return Err(CommandError::InvalidSchema);
                }
            }
        }
        let mut optional_positional_seen = false;
        for positional in &self.positionals {
            if !valid_name(&positional.name)
                || positional.name == "help"
                || !option_names.insert(positional.name.clone())
                || (optional_positional_seen && !positional.optional)
                || matches!(&positional.kind, ValueKind::Enumeration(values) if values.is_empty() || values.iter().any(|value| value.contains('\0')))
            {
                return Err(CommandError::InvalidSchema);
            }
            optional_positional_seen |= positional.optional;
        }
        for child in &self.subcommands {
            if child.name == "help" || !child_names.insert(child.name.clone()) {
                return Err(CommandError::InvalidSchema);
            }
            child.validate_into(option_names)?;
        }
        Ok(())
    }

    fn validate_references(
        &self,
        option_names: &std::collections::BTreeSet<String>,
    ) -> Result<(), CommandError> {
        for option in &self.options {
            if option
                .conflicts
                .iter()
                .chain(&option.requires_any)
                .any(|name| !option_names.contains(name))
            {
                return Err(CommandError::InvalidSchema);
            }
        }
        let mut group_names = std::collections::BTreeSet::new();
        for (name, options) in &self.exclusive_groups {
            if !valid_name(name)
                || !group_names.insert(name)
                || options.len() < 2
                || options.iter().any(|option| !option_names.contains(option))
            {
                return Err(CommandError::InvalidSchema);
            }
        }
        for child in &self.subcommands {
            child.validate_references(option_names)?;
        }
        Ok(())
    }

    fn collect_values(
        &self,
        matches: &clap::ArgMatches,
        values: &mut BTreeMap<String, ParsedValue>,
    ) -> Result<(), CommandError> {
        for option in &self.options {
            let value = match option.kind {
                None => ParsedValue::Flag(
                    matches
                        .try_get_one::<bool>(&option.name)
                        .map_err(|_| CommandError::InvalidArguments)?
                        .copied()
                        .unwrap_or(false),
                ),
                Some(ValueKind::String) if option.repeated => matches
                    .try_get_many::<String>(&option.name)
                    .map_err(|_| CommandError::InvalidArguments)?
                    .map(|items| ParsedValue::Strings(items.cloned().collect()))
                    .unwrap_or(ParsedValue::Absent),
                Some(ValueKind::String) | Some(ValueKind::Enumeration(_)) => matches
                    .try_get_one::<String>(&option.name)
                    .map_err(|_| CommandError::InvalidArguments)?
                    .cloned()
                    .map(ParsedValue::String)
                    .unwrap_or(ParsedValue::Absent),
                Some(ValueKind::OsString) if option.repeated => matches
                    .try_get_many::<std::ffi::OsString>(&option.name)
                    .map_err(|_| CommandError::InvalidArguments)?
                    .map(|items| ParsedValue::OsStrings(items.cloned().collect()))
                    .unwrap_or(ParsedValue::Absent),
                Some(ValueKind::OsString) => matches
                    .try_get_one::<std::ffi::OsString>(&option.name)
                    .map_err(|_| CommandError::InvalidArguments)?
                    .cloned()
                    .map(ParsedValue::OsString)
                    .unwrap_or(ParsedValue::Absent),
                Some(ValueKind::F64) => match matches
                    .try_get_one::<f64>(&option.name)
                    .map_err(|_| CommandError::InvalidArguments)?
                    .copied()
                {
                    Some(value) if value.is_finite() => ParsedValue::F64(value),
                    Some(_) => return Err(CommandError::InvalidArguments),
                    None => ParsedValue::Absent,
                },
                Some(ValueKind::U32) => matches
                    .try_get_one::<u32>(&option.name)
                    .map_err(|_| CommandError::InvalidArguments)?
                    .copied()
                    .map(ParsedValue::U32)
                    .unwrap_or(ParsedValue::Absent),
            };
            values.insert(option.name.clone(), value);
        }
        for positional in &self.positionals {
            let value = match &positional.kind {
                ValueKind::String | ValueKind::Enumeration(_) => matches
                    .try_get_one::<String>(&positional.name)
                    .map_err(|_| CommandError::InvalidArguments)?
                    .cloned()
                    .map(ParsedValue::String)
                    .unwrap_or(ParsedValue::Absent),
                ValueKind::OsString => matches
                    .try_get_one::<std::ffi::OsString>(&positional.name)
                    .map_err(|_| CommandError::InvalidArguments)?
                    .cloned()
                    .map(ParsedValue::OsString)
                    .unwrap_or(ParsedValue::Absent),
                ValueKind::F64 => match matches
                    .try_get_one::<f64>(&positional.name)
                    .map_err(|_| CommandError::InvalidArguments)?
                    .copied()
                {
                    Some(value) if value.is_finite() => ParsedValue::F64(value),
                    Some(_) => return Err(CommandError::InvalidArguments),
                    None => ParsedValue::Absent,
                },
                ValueKind::U32 => matches
                    .try_get_one::<u32>(&positional.name)
                    .map_err(|_| CommandError::InvalidArguments)?
                    .copied()
                    .map(ParsedValue::U32)
                    .unwrap_or(ParsedValue::Absent),
            };
            values.insert(positional.name.clone(), value);
        }
        Ok(())
    }

    fn validate_selected_relations(
        selected: &[&Self],
        explicit_options: &std::collections::BTreeSet<String>,
    ) -> Result<(), CommandError> {
        for command in selected {
            for option in &command.options {
                if explicit_options.contains(&option.name)
                    && option
                        .conflicts
                        .iter()
                        .any(|name| explicit_options.contains(name))
                {
                    return Err(CommandError::InvalidArguments);
                }
                if explicit_options.contains(&option.name)
                    && !option.requires_any.is_empty()
                    && !option
                        .requires_any
                        .iter()
                        .any(|name| explicit_options.contains(name))
                {
                    return Err(CommandError::InvalidArguments);
                }
            }
            for (_, options) in &command.exclusive_groups {
                if options
                    .iter()
                    .filter(|name| explicit_options.contains(*name))
                    .take(2)
                    .count()
                    > 1
                {
                    return Err(CommandError::InvalidArguments);
                }
            }
        }
        Ok(())
    }
}

fn explicit_option_names(
    words: &[std::ffi::OsString],
) -> Result<std::collections::BTreeSet<String>, CommandError> {
    let mut names = std::collections::BTreeSet::new();
    let mut options_enabled = true;
    for word in words.iter().skip(1) {
        let bytes = word.as_encoded_bytes();
        if options_enabled && bytes == b"--" {
            options_enabled = false;
        } else if options_enabled {
            if let Some(name) = bytes.strip_prefix(b"--") {
                let name = name.split(|byte| *byte == b'=').next().unwrap_or_default();
                if let Ok(name) = std::str::from_utf8(name) {
                    names.insert(name.to_owned());
                }
            }
        }
    }
    Ok(names)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_value(kind: &ValueKind, value: &str) -> bool {
    match kind {
        ValueKind::String => !value.contains('\0'),
        ValueKind::OsString => !value.contains('\0'),
        ValueKind::Enumeration(values) => values.iter().any(|candidate| candidate == value),
        ValueKind::F64 => value.parse::<f64>().is_ok_and(f64::is_finite),
        ValueKind::U32 => value.parse::<u32>().is_ok(),
    }
}

/// Parsed scalar values independent of the private parser backend.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedValue {
    /// A declared flag.
    Flag(bool),
    /// A declared value option.
    String(String),
    /// A declared lossless operating-system-string option or positional.
    OsString(std::ffi::OsString),
    /// A repeated value option, in command-line order.
    Strings(Vec<String>),
    /// A repeated declared lossless operating-system-string option.
    OsStrings(Vec<std::ffi::OsString>),
    /// A declared `f64` option or positional.
    F64(f64),
    /// A declared `u32` option or positional.
    U32(u32),
    /// A declared value option was absent and has no default.
    Absent,
}

/// A facade-owned parsed command line.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedCommand {
    path: Vec<String>,
    values: BTreeMap<String, ParsedValue>,
}

impl ParsedCommand {
    /// Selected command names including the root.
    pub fn command_path(&self) -> &[String] {
        &self.path
    }

    /// Read a declared boolean flag.
    pub fn flag(&self, name: &str) -> Option<bool> {
        match self.values.get(name) {
            Some(ParsedValue::Flag(value)) => Some(*value),
            _ => None,
        }
    }

    /// Read a declared string option or its default.
    pub fn value(&self, name: &str) -> Option<&str> {
        match self.values.get(name) {
            Some(ParsedValue::String(value)) => Some(value),
            _ => None,
        }
    }

    /// Read all values of a repeated option.
    pub fn values(&self, name: &str) -> Option<&[String]> {
        match self.values.get(name) {
            Some(ParsedValue::Strings(values)) => Some(values),
            _ => None,
        }
    }

    /// Read a declared operating-system-string option or positional.
    pub fn os_value(&self, name: &str) -> Option<&OsStr> {
        match self.values.get(name) {
            Some(ParsedValue::OsString(value)) => Some(value.as_os_str()),
            _ => None,
        }
    }

    /// Read all values of a repeated declared operating-system-string option.
    pub fn os_values(&self, name: &str) -> Option<&[std::ffi::OsString]> {
        match self.values.get(name) {
            Some(ParsedValue::OsStrings(values)) => Some(values),
            _ => None,
        }
    }

    /// Read a declared `f64` option or positional.
    pub fn f64(&self, name: &str) -> Option<f64> {
        match self.values.get(name) {
            Some(ParsedValue::F64(value)) => Some(*value),
            _ => None,
        }
    }

    /// Read a declared `u32` option or positional.
    pub fn u32(&self, name: &str) -> Option<u32> {
        match self.values.get(name) {
            Some(ParsedValue::U32(value)) => Some(*value),
            _ => None,
        }
    }
}

/// Parsing or schema failures without backend diagnostics or input echo.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CommandError {
    #[error("command schema is invalid")]
    InvalidSchema,
    #[error("command line contains a NUL character")]
    ContainsNul,
    #[error("command line contains a non-UTF-8 argument")]
    InvalidUtf8,
    #[error("command line has too many arguments")]
    TooManyArguments,
    #[error("command-line argument exceeds byte limit")]
    ArgumentTooLarge,
    #[error("command line exceeds byte limit")]
    InputTooLarge,
    #[error("invalid command-line arguments")]
    InvalidArguments,
}
