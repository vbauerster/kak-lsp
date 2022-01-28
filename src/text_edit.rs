use crate::context::*;
use crate::position::*;
use crate::types::*;
use crate::util::*;
use indoc::formatdoc;
#[cfg(test)]
use indoc::indoc;
use itertools::Itertools;
use lsp_types::*;
use ropey::{Rope, RopeSlice};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::io::FromRawFd;

pub trait TextEditish<T: TextEditish<T>> {
    fn text_edit(self) -> TextEdit;
    fn as_ref(&self) -> &TextEdit;
}

impl TextEditish<TextEdit> for TextEdit {
    fn text_edit(self) -> TextEdit {
        self
    }
    fn as_ref(&self) -> &TextEdit {
        self
    }
}

impl TextEditish<AnnotatedTextEdit> for AnnotatedTextEdit {
    fn text_edit(self) -> TextEdit {
        self.text_edit
    }
    fn as_ref(&self) -> &TextEdit {
        &self.text_edit
    }
}

impl TextEditish<OneOf<TextEdit, AnnotatedTextEdit>> for OneOf<TextEdit, AnnotatedTextEdit> {
    fn text_edit(self) -> TextEdit {
        match self {
            OneOf::Left(text_edit) => text_edit,
            OneOf::Right(annotated_text_edit) => annotated_text_edit.text_edit,
        }
    }
    fn as_ref(&self) -> &TextEdit {
        match self {
            OneOf::Left(text_edit) => text_edit,
            OneOf::Right(annotated_text_edit) => &annotated_text_edit.text_edit,
        }
    }
}

/// Apply text edits to the file pointed by uri either by asking Kakoune to modify corresponding
/// buffer or by editing file directly when it's not open in editor.
pub fn apply_text_edits(meta: &EditorMeta, uri: &Url, edits: Vec<TextEdit>, ctx: &Context) {
    apply_annotated_text_edits(meta, uri, edits, ctx)
}

/// Apply text edits to the file pointed by uri either by asking Kakoune to modify corresponding
/// buffer or by editing file directly when it's not open in editor.
pub fn apply_annotated_text_edits<T: TextEditish<T>>(
    meta: &EditorMeta,
    uri: &Url,
    edits: Vec<T>,
    ctx: &Context,
) {
    if let Some(document) = uri
        .to_file_path()
        .ok()
        .and_then(|path| path.to_str().and_then(|buffile| ctx.documents.get(buffile)))
    {
        let meta = meta.clone();
        match apply_text_edits_to_buffer(
            &meta.client,
            Some(uri),
            edits,
            &document.text,
            ctx.offset_encoding,
        ) {
            Some(cmd) => ctx.exec(meta, cmd),
            // Nothing to do, but sending command back to the editor is required to handle case when
            // editor is blocked waiting for response via fifo.
            None => ctx.exec(meta, "nop"),
        }
    } else if let Err(e) = apply_text_edits_to_file(uri, edits, ctx.offset_encoding) {
        error!("Failed to apply edits to file {} ({})", uri, e);
    }
}

pub fn apply_text_edits_to_file<T: TextEditish<T>>(
    uri: &Url,
    text_edits: Vec<T>,
    offset_encoding: OffsetEncoding,
) -> std::io::Result<()> {
    let path = uri.to_file_path().unwrap();
    let filename = path.to_str().unwrap();

    let path = std::ffi::CString::new(filename).unwrap();
    let mut stat;
    if unsafe {
        stat = std::mem::zeroed();
        libc::stat(path.as_ptr(), &mut stat)
    } != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("Failed to stat {}", filename),
        ));
    }

    let file = File::open(filename)?;
    let text = Rope::from_reader(BufReader::new(file))?;

    let (temp_path, temp_file) = {
        let template = format!("{}.XXXXXX", filename);
        let cstr = std::ffi::CString::new(template).unwrap();
        let ptr = cstr.into_raw();
        let temp_fd = unsafe { libc::mkstemp(ptr) };
        let cstr = unsafe { std::ffi::CString::from_raw(ptr) };
        let temp_fd = cvt(temp_fd)?;
        let temp_path = cstr.into_string().unwrap();
        let temp_file = unsafe { File::from_raw_fd(temp_fd) };
        (temp_path, temp_file)
    };
    fn apply_text_edits_to_file_impl<T: TextEditish<T>>(
        text: Rope,
        temp_file: File,
        text_edits: Vec<T>,
        offset_encoding: OffsetEncoding,
    ) -> Result<(), std::io::Error> {
        let mut output = BufWriter::new(temp_file);

        let text_len_lines = text.len_lines() as u64;
        let mut cursor = 0;

        for te in text_edits {
            let TextEdit {
                range: Range { start, end },
                new_text,
            } = te.as_ref();

            if start.line as u64 >= text_len_lines || end.line as u64 >= text_len_lines {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Text edit range extends past end of file.",
                ));
            }

            let start_offset = character_to_offset(
                offset_encoding,
                text.line(start.line as _),
                start.character as _,
            );
            let end_offset = character_to_offset(
                offset_encoding,
                text.line(end.line as _),
                end.character as _,
            );

            if start_offset.is_none() || end_offset.is_none() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Text edit range points past end of line.",
                ));
            }

            let start_char = text.line_to_char(start.line as _) + start_offset.unwrap();
            let end_char = text.line_to_char(end.line as _) + end_offset.unwrap();

            for chunk in text.slice(cursor..start_char).chunks() {
                output.write_all(chunk.as_bytes())?;
            }

            output.write_all(new_text.as_bytes())?;
            cursor = end_char;
        }

        for chunk in text.slice(cursor..).chunks() {
            output.write_all(chunk.as_bytes())?;
        }

        Ok(())
    }

    apply_text_edits_to_file_impl(text, temp_file, text_edits, offset_encoding)
        .and_then(|_| std::fs::rename(&temp_path, filename))
        .map(|_| unsafe {
            libc::chmod(path.as_ptr(), stat.st_mode);
        })
        .map_err(|e| {
            let _ = std::fs::remove_file(&temp_path);
            e
        })
}

// Adapted from std/src/sys/unix/mod.rs.
fn cvt(t: i32) -> std::io::Result<i32> {
    if t == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(t)
    }
}

fn character_to_offset(
    offset_encoding: OffsetEncoding,
    line: RopeSlice,
    character: usize,
) -> Option<usize> {
    match offset_encoding {
        OffsetEncoding::Utf8 => character_to_offset_utf_8_code_units(line, character),
        // Not a proper UTF-16 code units handling, but works within BMP
        OffsetEncoding::Utf16 => character_to_offset_utf_8_code_points(line, character),
    }
}

fn character_to_offset_utf_8_code_points(line: RopeSlice, character: usize) -> Option<usize> {
    if character < line.len_chars() {
        Some(character)
    } else {
        None
    }
}

fn character_to_offset_utf_8_code_units(line: RopeSlice, character: usize) -> Option<usize> {
    if character <= line.len_bytes() {
        Some(line.byte_to_char(character))
    } else {
        None
    }
}

fn byte_to_offset(
    offset_encoding: OffsetEncoding,
    line: RopeSlice,
    character: usize,
) -> Option<usize> {
    match offset_encoding {
        OffsetEncoding::Utf8 => byte_to_offset_utf_8_code_units(line, character),
        // Not a proper UTF-16 code units handling, but works within BMP
        OffsetEncoding::Utf16 => byte_to_offset_utf_8_code_points(line, character),
    }
}

fn byte_to_offset_utf_8_code_points(line: RopeSlice, character: usize) -> Option<usize> {
    if character < line.len_chars() {
        Some(line.char_to_byte(character))
    } else {
        None
    }
}

fn byte_to_offset_utf_8_code_units(line: RopeSlice, character: usize) -> Option<usize> {
    if character <= line.len_bytes() {
        Some(character)
    } else {
        None
    }
}
pub fn apply_text_edits_to_buffer<T: TextEditish<T>>(
    client: &Option<String>,
    uri: Option<&Url>,
    mut text_edits: Vec<T>,
    text: &Rope,
    offset_encoding: OffsetEncoding,
) -> Option<String> {
    // Empty text edits processed as a special case because Kakoune's `select` command
    // doesn't support empty arguments list.
    if text_edits.is_empty() {
        return None;
    }

    // Adjoin selections detection and Kakoune side editing relies on edits being ordered left to
    // right. Language servers usually send them such, but spec doesn't say anything about the order
    // hence we ensure it by sorting. It's important to use stable sort to handle properly cases
    // like multiple inserts in the same place.
    text_edits.sort_by_key(|x| {
        let range = x.as_ref().range;
        (range.start, range.end)
    });

    let mut offset = 0;

    let mut coalesced_edits: Vec<TextEdit> = vec![];
    for edit in text_edits {
        let edit = edit.text_edit();
        let Range { start, end } = edit.range;
        let start_line = text.get_line(start.line as _);
        let start_column = start_line.and_then(|start_line| {
            byte_to_offset(offset_encoding, start_line, start.character as _)
        });
        let start_offset = text.line_to_byte(start.line as _) + start_column.unwrap_or(0);
        let end_line = text.get_line(end.line as _);
        let end_column = end_line
            .and_then(|end_line| byte_to_offset(offset_encoding, end_line, end.character as _));
        let end_offset = text.line_to_byte(end.line as _) + end_column.unwrap_or(0);
        if offset == start_offset && !coalesced_edits.is_empty() {
            let last = coalesced_edits.last_mut().unwrap();
            assert!(start == last.range.end);
            last.range.end = end;
            last.new_text += &edit.new_text;
        } else {
            coalesced_edits.push(edit)
        }
        offset = end_offset;
    }

    let edits = coalesced_edits
        .into_iter()
        .filter(|text_edit| {
            // Drop redundant text edits because Kakoune treats them differently. Here's how
            //
            // 0. Assume we have two adjacent selections "foo" "bar".
            // 1. Use "Z" to save the two selection .
            // 2. Use "<space>" to select "foo"
            // 3. Type "|echo foo<ret>"
            // 4. Run "z" to restore the two selections. Observe that "foo" is still selected.
            //
            // If we repeat step 3 with any other text, running "z" will show that the first
            // selection goes away because it was merged into the second one. This is what our
            // logic to compute merged selections will do later. It doesn't account for Kakoune
            // optimizing redundant text edits, so just drop them here.
            let TextEdit { range, new_text } = text_edit.as_ref();
            // TODO Also drop redundant edits that span multiple lines.
            if range.start.line != range.end.line {
                return true;
            }
            let line = text.line(range.start.line as _);
            let start_byte =
                byte_to_offset(offset_encoding, line, range.start.character as _).unwrap();
            let end_byte = byte_to_offset(offset_encoding, line, range.end.character as _).unwrap();
            let bytes = line.bytes_at(start_byte);
            let contents = bytes.take(end_byte - start_byte).collect::<Vec<u8>>();
            let redundant = new_text.as_bytes() == contents;
            !redundant
        })
        .map(|text_edit| lsp_text_edit_to_kakoune(&text_edit, text, offset_encoding))
        .collect::<Vec<_>>();

    let selection_descs = edits
        .iter()
        .map(|edit| format!("{}", edit.range))
        .dedup()
        .join(" ");

    let mut apply_edits = edits
        .iter()
        .enumerate()
        .map(
            |(
                i,
                KakouneTextEdit {
                    new_text, command, ..
                },
            )| {
                let command = match command {
                    KakouneTextEditCommand::InsertBefore => "lsp-insert-before-selection",
                    KakouneTextEditCommand::Replace => "lsp-replace-selection",
                };
                let command = formatdoc!(
                    "exec \"z{}<space>\"
                     {} {}",
                    if i > 0 {
                        format!("{})", i)
                    } else {
                        String::new()
                    },
                    command,
                    editor_quote(new_text)
                );
                command
            },
        )
        .join("\n");

    let maybe_buffile = uri
        .and_then(|uri| uri.to_file_path().ok())
        .and_then(|path| path.to_str().map(|buffile| buffile.to_string()));

    if !selection_descs.is_empty() {
        apply_edits = formatdoc!(
            "select {}
             exec -save-regs \"\" Z
             {}",
            selection_descs,
            apply_edits
        );
    }

    let client = match client {
        None => {
            return Some(
                maybe_buffile
                    .map(|buffile| {
                        format!(
                            "eval -buffer {} -save-regs ^ {}",
                            editor_quote(&buffile),
                            editor_quote(&apply_edits)
                        )
                    })
                    .unwrap_or_else(|| {
                        format!("eval -draft -save-regs ^ {}", editor_quote(&apply_edits))
                    }),
            );
        }
        Some(client) => client,
    };

    // Go to the target file, in case it's not active.
    let apply_edits = maybe_buffile
        .map(|buffile| {
            formatdoc!(
                "edit -existing -- {}
                 {}",
                editor_quote(&buffile),
                &apply_edits
            )
        })
        .unwrap_or(apply_edits);

    Some(format!(
        "eval -client {} -draft -save-regs ^ {}",
        client,
        &editor_quote(&apply_edits)
    ))
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum KakouneTextEditCommand {
    InsertBefore,
    Replace,
}

struct KakouneTextEdit {
    range: KakouneRange,
    new_text: String,
    command: KakouneTextEditCommand,
}

fn lsp_text_edit_to_kakoune<T: TextEditish<T>>(
    text_edit: &T,
    text: &Rope,
    offset_encoding: OffsetEncoding,
) -> KakouneTextEdit {
    let TextEdit { range, new_text } = text_edit.as_ref();
    let Range { start, end } = range;
    let insert = start.line == end.line && start.character == end.character;

    let range = lsp_range_to_kakoune(range, text, offset_encoding);

    let command = if insert {
        KakouneTextEditCommand::InsertBefore
    } else {
        KakouneTextEditCommand::Replace
    };

    KakouneTextEdit {
        range,
        new_text: new_text.to_string(),
        command,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        new_text: &str,
    ) -> OneOf<TextEdit, AnnotatedTextEdit> {
        OneOf::Left(TextEdit {
            range: Range {
                start: Position {
                    line: start_line,
                    character: start_character,
                },
                end: Position {
                    line: end_line,
                    character: end_character,
                },
            },
            new_text: new_text.to_string(),
        })
    }

    #[test]
    pub fn apply_text_edits_to_buffer_issue_521() {
        let text_edits = vec![
            edit(0, 4, 0, 7, "std"),
            edit(0, 7, 0, 9, ""),
            edit(0, 9, 0, 12, ""),
            edit(0, 14, 0, 21, "ffi"),
            edit(0, 21, 0, 21, "::"),
            edit(0, 21, 0, 21, "{CStr, CString}"),
        ];
        let buffer = Rope::from_str("use std::ffi::CString;");
        let result =
            apply_text_edits_to_buffer(&None, None, text_edits, &buffer, OffsetEncoding::Utf8);
        let expected = indoc!(
            r#"eval -draft -save-regs ^ 'select 1.5,1.12 1.15,1.21
               exec -save-regs "" Z
               exec "z<space>"
               lsp-replace-selection ''std''
               exec "z1)<space>"
               lsp-replace-selection ''ffi::{CStr, CString}'''"#
        )
        .to_string();
        assert_eq!(result, Some(expected));
    }

    #[test]
    pub fn apply_text_edits_to_buffer_insert_adjacent_to_replace() {
        let text_edits = vec![edit(0, 1, 0, 1, "inserted"), edit(0, 2, 0, 3, "replaced")];
        let buffer = Rope::from_str("0123");
        let result =
            apply_text_edits_to_buffer(&None, None, text_edits, &buffer, OffsetEncoding::Utf8);
        let expected = indoc!(
            r#"eval -draft -save-regs ^ 'select 1.2,1.2 1.3,1.3
               exec -save-regs "" Z
               exec "z<space>"
               lsp-insert-before-selection ''inserted''
               exec "z1)<space>"
               lsp-replace-selection ''replaced'''"#
        )
        .to_string();
        assert_eq!(result, Some(expected));
    }

    #[test]
    pub fn apply_text_edits_to_buffer_issue_527() {
        let text_edits = vec![
            edit(0, 4, 0, 9, "if"),
            edit(0, 10, 0, 13, "let"),
            edit(0, 13, 0, 13, " "),
            edit(0, 13, 0, 13, "Test::Foo"),
            edit(0, 13, 0, 13, " "),
            edit(0, 13, 0, 13, "="),
            edit(0, 13, 0, 13, " "),
            edit(0, 13, 0, 13, "foo"),
            edit(1, 8, 1, 12, "println"),
            edit(1, 12, 1, 14, ""),
        ];

        let buffer = Rope::from_str(
            "    match foo {
        Test::Foo => println!(\"foo\"),
        _ => {}
    }",
        );
        let result =
            apply_text_edits_to_buffer(&None, None, text_edits, &buffer, OffsetEncoding::Utf8);
        let expected = indoc!(
            r#"eval -draft -save-regs ^ 'select 1.5,1.9 1.11,1.13 2.9,2.14
               exec -save-regs "" Z
               exec "z<space>"
               lsp-replace-selection ''if''
               exec "z1)<space>"
               lsp-replace-selection ''let Test::Foo = foo''
               exec "z2)<space>"
               lsp-replace-selection ''println'''"#
        )
        .to_string();
        assert_eq!(result, Some(expected));
    }

    #[test]
    pub fn apply_text_edits_to_buffer_merge_imports() {
        let text_edits = vec![
                edit(0, 4, 0, 7, "std"),
                edit(0, 7, 0, 9, ""),
                edit(0, 9, 0, 13, ""),
                edit(0, 13, 0, 15, ""),
                edit(0, 15, 0, 19, ""),
                edit(0, 19, 0, 19, "::"),
                edit(0, 19, 0, 19, "{path::Path, process::Stdio}"),
                edit(1, 0, 1, 24, "\n"),
                edit(1, 24, 1, 24, "fn main() {\n    let matches = App::new(\"kak-lsp\").get_matches();\n\n    if matches.is_present(\"kakoune\") {}\n}"),
                edit(3, 3, 3, 7, "kakoune"),
                edit(4, 8, 4, 15, "script"),
                edit(4, 15, 4, 15, ":"),
                edit(4, 16, 4, 16, "&str"),
                edit(4, 16, 4, 16, " "),
                edit(4, 18, 4, 21, "include_str"),
                edit(4, 21, 4, 23, ""),
                edit(4, 23, 4, 26, ""),
                edit(4, 26, 4, 37, ""),
                edit(4, 37, 4, 38, "!"),
                edit(4, 38, 4, 49, "("),
                edit(4, 49, 4, 49, "\"../rc/lsp.kak\""),
                edit(4, 49, 4, 49, ")"),
                edit(4, 49, 4, 51, ""),
                edit(4, 52, 6, 4, "\n    "),
                edit(6, 4, 6, 6, "let"),
                edit(6, 7, 6, 14, "args"),
                edit(6, 14, 6, 15, ""),
                edit(6, 15, 6, 25, ""),
                edit(6, 25, 6, 36, ""),
                edit(6, 37, 6, 39, "="),
                edit(6, 39, 6, 39, " "),
                edit(6, 39, 6, 39, "env::args().skip(1)"),
                edit(6, 39, 6, 39, ";"),
                edit(7, 1, 9, 0, "\n"),
                edit(9, 0, 12, 1, ""),
                edit(12, 1, 13, 0, ""),
        ];
        let buffer = Rope::from_str(indoc!(
            r#"use std::path::Path;
               use std::process::Stdio;

               fn main() {
                   let matches = App::new("kak-lsp").get_matches();

                   if matches.is_present("kakoune") {}
               }

               fn kakoune() {
                   let script: &str = include_str!("../rc/lsp.kak");
                   let args = env::args().skip(1);
               }
               "#
        ));
        let result =
            apply_text_edits_to_buffer(&None, None, text_edits, &buffer, OffsetEncoding::Utf8);

        let expected = indoc!(
            r#"eval -draft -save-regs ^ 'select 1.5,1.19 2.1,2.24 4.4,4.7 5.9,5.15 5.17,5.17 5.19,5.51 5.53,7.6 7.8,7.36 7.38,7.39 8.2,13.1000000
               exec -save-regs "" Z
               exec "z<space>"
               lsp-replace-selection ''std::{path::Path, process::Stdio}''
               exec "z1)<space>"
               lsp-replace-selection ''
               fn main() {
                   let matches = App::new("kak-lsp").get_matches();

                   if matches.is_present("kakoune") {}
               }''
               exec "z2)<space>"
               lsp-replace-selection ''kakoune''
               exec "z3)<space>"
               lsp-replace-selection ''script:''
               exec "z4)<space>"
               lsp-insert-before-selection ''&str ''
               exec "z5)<space>"
               lsp-replace-selection ''include_str!("../rc/lsp.kak")''
               exec "z6)<space>"
               lsp-replace-selection ''
                   let''
               exec "z7)<space>"
               lsp-replace-selection ''args''
               exec "z8)<space>"
               lsp-replace-selection ''= env::args().skip(1);''
               exec "z9)<space>"
               lsp-replace-selection ''
               '''"#
        ).to_string();
        assert_eq!(result, Some(expected));
    }
}
