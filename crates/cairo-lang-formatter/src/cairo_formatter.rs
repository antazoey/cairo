use std::fmt::{Debug, Display};
use std::fs;
use std::io::{Read, stdin};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use cairo_lang_diagnostics::FormattedDiagnosticEntry;
use cairo_lang_filesystem::db::FilesGroup;
use cairo_lang_filesystem::ids::{CAIRO_FILE_EXTENSION, FileId, FileKind, FileLongId, VirtualFile};
use cairo_lang_parser::utils::{SimpleParserDatabase, get_syntax_root_and_diagnostics};
use cairo_lang_utils::Intern;
use diffy::{PatchFormatter, create_patch};
use ignore::WalkBuilder;
use ignore::types::TypesBuilder;
use thiserror::Error;

use crate::{CAIRO_FMT_IGNORE, FormatterConfig, get_formatted_file};

/// A struct encapsulating the changes made by the formatter in a single file.
///
/// This struct implements [`Display`] and [`Debug`] traits, showing differences between
/// the original and modified file content.
#[derive(Clone)]
pub struct FileDiff {
    pub original: String,
    pub formatted: String,
}

impl FileDiff {
    pub fn display_colored(&self) -> FileDiffColoredDisplay<'_> {
        FileDiffColoredDisplay { diff: self }
    }
}

impl Display for FileDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", create_patch(&self.original, &self.formatted))
    }
}

impl Debug for FileDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "FileDiff({self})")
    }
}

/// A helper struct for displaying a file diff with colored output.
///
/// This struct implements a [`Display`] trait, so it can be used with `format!` and `println!`.
/// If you prefer output without colors, use [`FileDiff`] instead.
pub struct FileDiffColoredDisplay<'a> {
    diff: &'a FileDiff,
}

impl Display for FileDiffColoredDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let patch = create_patch(&self.diff.original, &self.diff.formatted);
        let patch_formatter = PatchFormatter::new().with_color();
        let formatted_patch = patch_formatter.fmt_patch(&patch);
        formatted_patch.fmt(f)
    }
}

impl Debug for FileDiffColoredDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "FileDiffColoredDisplay({:?})", self.diff)
    }
}

/// An output from file formatting.
///
/// Differentiates between already formatted files and files that differ after formatting.
/// Contains the original file content and the formatted file content.
#[derive(Debug)]
pub enum FormatOutcome {
    Identical(String),
    DiffFound(FileDiff),
}

impl FormatOutcome {
    pub fn into_output_text(self) -> String {
        match self {
            FormatOutcome::Identical(original) => original,
            FormatOutcome::DiffFound(diff) => diff.formatted,
        }
    }
}

/// An error thrown while trying to format Cairo code.
#[derive(Debug, Error)]
pub enum FormattingError {
    /// A parsing error has occurred. See diagnostics for context.
    #[error(transparent)]
    ParsingError(ParsingError),
    /// All other errors.
    #[error(transparent)]
    Error(#[from] anyhow::Error),
}

/// Parsing error representation with diagnostic entries.
#[derive(Debug, Error)]
pub struct ParsingError(Vec<FormattedDiagnosticEntry>);

impl ParsingError {
    pub fn iter(&self) -> impl Iterator<Item = &FormattedDiagnosticEntry> {
        self.0.iter()
    }
}

impl From<ParsingError> for Vec<FormattedDiagnosticEntry> {
    fn from(error: ParsingError) -> Self {
        error.0
    }
}

impl From<Vec<FormattedDiagnosticEntry>> for ParsingError {
    fn from(diagnostics: Vec<FormattedDiagnosticEntry>) -> Self {
        Self(diagnostics)
    }
}

impl Display for ParsingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for entry in &self.0 {
            writeln!(f, "{entry}")?;
        }
        Ok(())
    }
}

/// A struct used to indicate that the formatter input should be read from stdin.
/// Implements the [`FormattableInput`] trait.
pub struct StdinFmt;

/// A trait for types that can be used as input for the cairo formatter.
pub trait FormattableInput {
    /// Converts the input to a [`FileId`] that can be used by the formatter.
    fn to_file_id<'a, 'db: 'a>(&self, db: &'db dyn FilesGroup) -> Result<FileId<'a>>;
    /// Overwrites the content of the input with the given string.
    fn overwrite_content(&self, _content: String) -> Result<()>;
}

impl FormattableInput for &Path {
    fn to_file_id<'a, 'db: 'a>(&self, db: &'db dyn FilesGroup) -> Result<FileId<'a>> {
        Ok(FileId::new_on_disk(db, PathBuf::from(self)))
    }
    fn overwrite_content(&self, content: String) -> Result<()> {
        fs::write(self, content)?;
        Ok(())
    }
}

impl FormattableInput for String {
    fn to_file_id<'a, 'db: 'a>(&self, db: &'db dyn FilesGroup) -> Result<FileId<'a>> {
        Ok(FileLongId::Virtual(VirtualFile {
            parent: None,
            name: "string_to_format".into(),
            content: self.clone().into(),
            code_mappings: [].into(),
            kind: FileKind::Module,
            original_item_removed: false,
        })
        .intern(db))
    }

    fn overwrite_content(&self, _content: String) -> Result<()> {
        Ok(())
    }
}

impl FormattableInput for StdinFmt {
    fn to_file_id<'a, 'db: 'a>(&self, db: &'db dyn FilesGroup) -> Result<FileId<'a>> {
        let mut buffer = String::new();
        stdin().read_to_string(&mut buffer)?;
        Ok(FileLongId::Virtual(VirtualFile {
            parent: None,
            name: "<stdin>".into(),
            content: buffer.into(),
            code_mappings: [].into(),
            kind: FileKind::Module,
            original_item_removed: false,
        })
        .intern(db))
    }
    fn overwrite_content(&self, content: String) -> Result<()> {
        print!("{content}");
        Ok(())
    }
}

fn format_input(
    input: &dyn FormattableInput,
    config: &FormatterConfig,
) -> Result<FormatOutcome, FormattingError> {
    let db = SimpleParserDatabase::default();
    let db_ref = &db;
    let file_id = input.to_file_id(db_ref).context("Unable to create virtual file.")?;
    let original_text = db_ref
        .file_content(file_id)
        .ok_or_else(|| anyhow!("Unable to read from input."))?
        .long(db_ref)
        .as_ref();
    let (syntax_root, diagnostics) = get_syntax_root_and_diagnostics(&db, file_id, original_text);
    if diagnostics.check_error_free().is_err() {
        return Err(FormattingError::ParsingError(
            diagnostics.format_with_severity(&db, &Default::default()).into(),
        ));
    }
    let formatted_text = get_formatted_file(&db, &syntax_root, config.clone());

    if formatted_text == original_text {
        Ok(FormatOutcome::Identical(original_text.to_string()))
    } else {
        let diff = FileDiff { original: original_text.to_string(), formatted: formatted_text };
        Ok(FormatOutcome::DiffFound(diff))
    }
}

/// A struct for formatting cairo files.
///
/// The formatter can operate on all types implementing the [`FormattableInput`] trait.
/// Allows formatting in place, and for formatting to a string.
#[derive(Debug)]
pub struct CairoFormatter {
    formatter_config: FormatterConfig,
}

impl CairoFormatter {
    pub fn new(formatter_config: FormatterConfig) -> Self {
        Self { formatter_config }
    }

    /// Returns a preconfigured `ignore::WalkBuilder` for the given path.
    ///
    /// Can be used for recursively formatting a directory under given path.
    pub fn walk(&self, path: &Path) -> WalkBuilder {
        let mut builder = WalkBuilder::new(path);
        builder.add_custom_ignore_filename(CAIRO_FMT_IGNORE);
        builder.follow_links(false);
        builder.skip_stdout(true);

        let mut types_builder = TypesBuilder::new();
        types_builder.add(CAIRO_FILE_EXTENSION, &format!("*.{CAIRO_FILE_EXTENSION}")).unwrap();
        types_builder.select(CAIRO_FILE_EXTENSION);
        builder.types(types_builder.build().unwrap());

        builder
    }

    /// Formats the path in place, writing changes to the files.
    /// The ['FormattableInput'] trait implementation defines the method for persisting changes.
    pub fn format_in_place(
        &self,
        input: &dyn FormattableInput,
    ) -> Result<FormatOutcome, FormattingError> {
        match format_input(input, &self.formatter_config)? {
            FormatOutcome::DiffFound(diff) => {
                // Persist changes.
                input.overwrite_content(diff.formatted.clone())?;
                Ok(FormatOutcome::DiffFound(diff))
            }
            FormatOutcome::Identical(original) => Ok(FormatOutcome::Identical(original)),
        }
    }

    /// Formats the path and returns the formatted string.
    /// No changes are persisted. The original file is not modified.
    pub fn format_to_string(
        &self,
        input: &dyn FormattableInput,
    ) -> Result<FormatOutcome, FormattingError> {
        format_input(input, &self.formatter_config)
    }
}
