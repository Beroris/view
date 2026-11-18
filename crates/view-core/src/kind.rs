//! What sort of file something is, for choosing how to show it: a picture, something to play, a
//! PDF, text of some kind, a 3D model, or none of those. Pictures are [`crate::formats`]'
//! business; this is everything around them.

/// How a file is shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Image,
    Video,
    Audio,
    Pdf,
    /// A 3D model: STL, 3MF or OBJ.
    Model,
    Text(TextKind),
    /// A Word, Excel or PowerPoint file, or a zip: read for its words or what it holds.
    Document(DocKind),
    /// A font file: TrueType or OpenType.
    Font,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DocKind {
    Word,
    Sheet,
    Slides,
    /// A zip, listed.
    Archive,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextKind {
    Plain,
    Markdown,
    Json,
    /// Values split by this byte: `,`, `;` or a tab.
    Table(u8),
    Code(Lang),
}

/// The languages View colours. Close relatives share one: the colouring only needs comments,
/// strings, numbers and keywords right.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Rust,
    C,
    CSharp,
    Java,
    Kotlin,
    Swift,
    Go,
    Script,
    Python,
    Ruby,
    Shell,
    PowerShell,
    Batch,
    Lua,
    Sql,
    Markup,
    Css,
    Yaml,
    Ini,
    Diff,
    /// Anything else that's clearly code: `#` and `//` comments, strings and numbers only.
    Generic,
}

pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "asf", "3gp", "3g2", "mpg", "mpeg", "m2ts", "ogv"];
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus", "wma", "aif", "aiff", "aifc", "alac", "m4b", "amr"];
/// 3D models. `.obj` is also used for compiled code, so the preview checks it's text first.
pub const MODEL_EXTENSIONS: &[&str] = &["stl", "3mf", "obj"];
/// Extensions shared by video and code: MPEG transport streams and TypeScript. The extension
/// says code; the preview looks inside to tell.
pub const VIDEO_OR_CODE: &[&str] = &["ts", "mts"];

/// The kind an extension says, before looking inside. `image` says whether it's a picture this
/// machine can decode.
pub fn from_extension(ext: &str, image: impl Fn(&str) -> bool) -> Kind {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    let e = ext.as_str();
    if image(e) {
        return Kind::Image;
    }
    if VIDEO_EXTENSIONS.contains(&e) {
        return Kind::Video;
    }
    if AUDIO_EXTENSIONS.contains(&e) {
        return Kind::Audio;
    }
    if MODEL_EXTENSIONS.contains(&e) {
        return Kind::Model;
    }
    if e == "pdf" {
        return Kind::Pdf;
    }
    if let Some(d) = doc_kind(e) {
        return Kind::Document(d);
    }
    if matches!(e, "ttf" | "otf" | "ttc" | "otc") {
        return Kind::Font;
    }
    match text_kind(e) {
        Some(t) => Kind::Text(t),
        None => Kind::Other,
    }
}

/// Office files and zips the preview reads itself. The old binary Office formats (.doc, .xls,
/// .ppt) aren't among them.
fn doc_kind(e: &str) -> Option<DocKind> {
    Some(match e {
        "docx" | "docm" | "dotx" | "dotm" => DocKind::Word,
        "xlsx" | "xlsm" | "xltx" | "xltm" => DocKind::Sheet,
        "pptx" | "pptm" | "ppsx" | "ppsm" | "potx" | "potm" => DocKind::Slides,
        "zip" | "jar" | "nupkg" | "vsix" | "apk" | "xpi" => DocKind::Archive,
        _ => return None,
    })
}

/// Whether an extension is something View plays.
pub fn is_media(ext: &str) -> bool {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    VIDEO_EXTENSIONS.contains(&e.as_str()) || AUDIO_EXTENSIONS.contains(&e.as_str())
}

/// Text files known by their whole name rather than an extension.
pub fn from_name(name: &str) -> Option<TextKind> {
    let n = name.to_ascii_lowercase();
    Some(match n.as_str() {
        "makefile" | "gnumakefile" | "dockerfile" | "containerfile" | "jenkinsfile" | "vagrantfile" | "rakefile" | "gemfile" | "procfile" | "justfile" => {
            TextKind::Code(if n == "rakefile" || n == "gemfile" || n == "vagrantfile" { Lang::Ruby } else { Lang::Shell })
        }
        ".bashrc" | ".bash_profile" | ".zshrc" | ".profile" | ".bash_aliases" => TextKind::Code(Lang::Shell),
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".dockerignore" | ".npmignore" | ".editorconfig" | ".npmrc" | ".gitconfig" | ".env" => TextKind::Code(Lang::Ini),
        "license" | "licence" | "copying" | "readme" | "authors" | "contributors" | "changelog" | "notice" | "todo" => TextKind::Plain,
        _ => return None,
    })
}

fn text_kind(e: &str) -> Option<TextKind> {
    use Lang::*;
    let code = |l| Some(TextKind::Code(l));
    match e {
        "txt" | "text" | "log" | "nfo" | "diz" | "srt" | "vtt" | "sub" | "me" | "1st" | "lst" | "out" => Some(TextKind::Plain),
        "md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdx" | "rmd" => Some(TextKind::Markdown),
        "json" | "jsonc" | "json5" | "geojson" | "webmanifest" | "har" | "ipynb" | "jsonl" | "ndjson" => Some(TextKind::Json),
        "csv" => Some(TextKind::Table(b',')),
        "tsv" | "tab" => Some(TextKind::Table(b'\t')),
        "rs" => code(Rust),
        "c" | "h" | "cpp" | "cc" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "inl" | "ino" | "m" | "mm" | "cu" | "glsl" | "hlsl" | "frag" | "vert" | "metal" | "zig" => code(C),
        "cs" | "csx" => code(CSharp),
        "java" | "scala" | "groovy" | "gradle" | "dart" => code(Java),
        "kt" | "kts" => code(Kotlin),
        "swift" => code(Swift),
        "go" => code(Go),
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx" | "es6" => code(Script),
        "py" | "pyw" | "pyi" | "pyx" | "gyp" | "bzl" | "star" => code(Python),
        "rb" | "rake" | "gemspec" | "erb" | "cr" => code(Ruby),
        "sh" | "bash" | "zsh" | "fish" | "ksh" | "command" | "mk" | "mak" | "cmake" | "nix" => code(Shell),
        "ps1" | "psm1" | "psd1" => code(PowerShell),
        "bat" | "cmd" => code(Batch),
        "lua" => code(Lua),
        "sql" | "psql" | "mysql" => code(Sql),
        "html" | "htm" | "xhtml" | "xml" | "xaml" | "xsl" | "xslt" | "xsd" | "svg" | "plist" | "csproj" | "vbproj" | "fsproj" | "vcxproj" | "props" | "targets" | "nuspec" | "manifest" | "resx" | "vue" | "svelte" | "astro" | "wxs" | "kml" | "gpx" | "rss" | "atom" | "opml" | "storyboard" => code(Markup),
        "css" | "scss" | "sass" | "less" | "styl" => code(Css),
        "yaml" | "yml" => code(Yaml),
        "toml" | "ini" | "cfg" | "conf" | "config" | "properties" | "prefs" | "reg" | "inf" | "iss" | "desktop" | "env" | "lock" | "sln" | "url" | "gitconfig" | "editorconfig" => code(Ini),
        "diff" | "patch" => code(Diff),
        "r" | "pl" | "pm" | "t" | "hs" | "lhs" | "ex" | "exs" | "erl" | "hrl" | "clj" | "cljs" | "edn" | "elm" | "fs" | "fsx" | "fsi" | "ml" | "mli" | "vb" | "vbs" | "bas" | "asm" | "s" | "nasm" | "tf" | "hcl" | "proto" | "graphql" | "gql" | "jl" | "nim" | "v" | "sv" | "vhd" | "vhdl" | "tex" | "sty" | "bib" | "awk" | "sed" | "tcl" | "pas" | "pp" | "d" | "f" | "f90" | "f95" | "cob" | "cbl" | "lisp" | "el" | "scm" | "rkt" | "hx" | "wgsl" | "sol" | "pro" | "qml" | "ahk" | "au3" | "nsi" | "nsh" | "cshtml" | "razor" | "php" | "twig" | "hbs" | "mustache" | "liquid" | "j2" | "jinja" => {
            code(Generic)
        }
        _ => None,
    }
}

/// What a script's `#!` line says it's written in.
pub fn from_shebang(first_line: &str) -> Option<Lang> {
    let l = first_line.strip_prefix("#!")?;
    Some(if l.contains("python") {
        Lang::Python
    } else if l.contains("ruby") {
        Lang::Ruby
    } else if l.contains("node") || l.contains("deno") || l.contains("bun") {
        Lang::Script
    } else if l.contains("pwsh") || l.contains("powershell") {
        Lang::PowerShell
    } else if l.contains("lua") {
        Lang::Lua
    } else {
        Lang::Shell
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_pick_kinds() {
        let img = |e: &str| e == "jpg";
        assert_eq!(from_extension("JPG", img), Kind::Image);
        assert_eq!(from_extension("mp4", img), Kind::Video);
        assert_eq!(from_extension(".flac", img), Kind::Audio);
        assert_eq!(from_extension("pdf", img), Kind::Pdf);
        assert_eq!(from_extension("md", img), Kind::Text(TextKind::Markdown));
        assert_eq!(from_extension("rs", img), Kind::Text(TextKind::Code(Lang::Rust)));
        assert_eq!(from_extension("tsv", img), Kind::Text(TextKind::Table(b'\t')));
        assert_eq!(from_extension("docx", img), Kind::Document(DocKind::Word));
        assert_eq!(from_extension("XLSX", img), Kind::Document(DocKind::Sheet));
        assert_eq!(from_extension("zip", img), Kind::Document(DocKind::Archive));
        assert_eq!(from_extension("otf", img), Kind::Font);
        assert_eq!(from_extension("doc", img), Kind::Other);
        assert_eq!(from_extension("STL", img), Kind::Model);
        assert_eq!(from_extension("ts", img), Kind::Text(TextKind::Code(Lang::Script)));
        assert!(is_media("MKV") && is_media("mp3") && !is_media("png"));
    }

    #[test]
    fn names_and_shebangs() {
        assert_eq!(from_name("Makefile"), Some(TextKind::Code(Lang::Shell)));
        assert_eq!(from_name("LICENSE"), Some(TextKind::Plain));
        assert_eq!(from_name("photo"), None);
        assert_eq!(from_shebang("#!/usr/bin/env python3"), Some(Lang::Python));
        assert_eq!(from_shebang("#!/bin/sh"), Some(Lang::Shell));
        assert_eq!(from_shebang("hello"), None);
    }
}
