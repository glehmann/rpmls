# rpmls

A language server for RPM spec files: the findings of
[rpmcrab](https://github.com/plusky/rpmcrab) or
[rpmlint](https://github.com/rpm-software-management/rpmlint) as the spec is
typed, and the editor features of a syntax tree, from
[tree-sitter-rpmspec](https://gitlab.com/cryptomilk/tree-sitter-rpmspec).

rpmls never asks rpm to expand the spec, which would run its shell and Lua
code: what it shows of the macros comes from their definitions. Only rpmlint,
when chosen, has rpm parse the spec.

## Features

- **Diagnostics**: the findings of the linter, 300 ms after the last change,
  on the lines they are about, without saving the spec.
  - Those of all the specs of the workspace too, as they are on disk, for
    the workspace diagnostics of the editor, like Helix's `space-D`: up to
    three directories deep, but in hidden directories and `target`, linted
    in the background, after the open specs.
  - Linted again when the editor reports a change of a spec, of an rpmlintrc
    file, or of the configuration, even out of the editor, like a
    `git checkout`, when it watches the files, like Helix.
  - rpmcrab, the default, reads the buffer from its stdin, without having rpm
    parse it (`--no-spec-parse`): about 0.06 s a spec.
  - rpmlint (`--linter rpmlint`) lints a copy of the buffer, in a directory
    linking to the other files of the spec's, for `%include %{SOURCE1}`:
    about 0.2 s. rpm parses it, running its code as it is typed, so run it in
    a container, without network nor write access to the project.
  - The rpmlintrc files beside the spec, and the configuration and filters of
    the project, with `--rpmlint-config` and `--rpmlintrc-dir`.
- **Hover**:
  - the linter's explanation of the findings on the line (`--explain`);
  - rpm's manual (`spec.md`) for tags, sections, scriptlets, conditionals and
    `%files` directives;
  - the definition of a macro, from the spec, or from rpm's macro files, with
    the file it is in.
- **Outline**: the sections, with the macros each defines, including those in
  conditionals.
- **Folding**: sections, conditionals, changelog entries, multi-line macro
  definitions and comment blocks.
- **Macros**:
  - go to definition, in the spec, or in rpm's macro files;
  - references and highlights, of every way a macro is written: `%name`,
    `%{name}`, `%{?name}`, `%{!?name:…}`, and in conditions;
  - rename, of the macros the spec defines.
- **Packages**, across the specs of the workspace:
  - go to definition on a dependency, like `BuildRequires: foo-devel`, to the
    spec building it: its `Name`, a `%package`, or a `Provides`;
  - references of a package name, built or required: the specs depending
    on it.

  Their simple macros are substituted, like `%{name}-libs`.
- **Completion**, only where each can be written:
  - preamble tags at the start of the lines of the preamble and `%package`
    sections, with the next free `SourceN:` and `PatchN:`, and rpm's
    deprecated tags marked so;
  - section, scriptlet, conditional and macro definition keywords after `%`
    at the start of a line, and the directives of `%files` in it;
  - macro names after `%` and `%{`, those of the spec and rpm's;
  - nothing in comments and the changelog.
- **Links**:
  - `Source` and `Patch` tags, and the entries of `%sourcelist` and
    `%patchlist`, to the local file, beside the spec or in `../SOURCES`, else
    to their URL;
  - `URL`, `BugURL` and `VCS` to the web.

  Their simple macros, one-line `%define` and `%global` values and the tags
  rpm makes macros of, like `%{name}` and `%{version}`, are substituted; a
  target with another macro gets no link.
- **Code actions**:
  - suppress a finding, with an `addFilter` in the rpmlintrc the linter loads
    for the spec, created when there is none: rpmls writes it, rather than
    the editor, which would leave it in a buffer to save, and lints the open
    specs again;
  - add a changelog entry at the top of `%changelog`, with today's date, rpm's
    `%packager`, or else git's user, and the spec's `[epoch:]version-release`
    when its simple macros resolve it; none with `%autochangelog`.

## Options

```
rpmls [--macro-cache DIR] [--spec-doc FILE] [--linter rpmcrab|rpmlint]
      [--rpmlint-config PATH]... [--rpmlintrc-dir DIR]
```

- `--macro-cache DIR`: copy the macro files go to definition points at to
  `DIR`, at their full path. When rpmls runs in a container, `DIR` is a
  directory shared with the host at the same path, for the editor to open
  them.
- `--spec-doc FILE`: rpm's manual, `spec.md`, when it is not installed, as in
  containers installed without documentation.
- `--linter rpmcrab|rpmlint`: the linter, rpmcrab by default.
- `--rpmlint-config PATH`: a configuration file of the linter, or a directory
  of them (`-c`).
- `--rpmlintrc-dir DIR`: a directory of the filters of each package,
  `<package>.rpmlintrc`, the package named after the directory of its spec,
  loaded after the rpmlintrc files beside the spec. The suppressed findings
  go there.

`--stdio` is accepted and ignored: stdio is the only transport.

## In containers

rpmls works on the system it runs in: rpm's macros and its macro path, the
linter and its configuration. To work on the specs of another distribution,
run it in a container of it, with the specs mounted at the same path as on
the host, for the paths of the editor to be those of the container, and a
`--macro-cache` directory shared likewise. With podman,
`--userns=keep-id` gives it the files of the user.

[xcraft](https://github.com/glehmann/xcraft) runs it this way:
`xcraft lsp`, from the directory of the project.

## Editors

With Helix, in `languages.toml`:

```toml
[language-server.rpmls]
command = "rpmls"

[[language]]
name = "rpmspec"
language-servers = ["rpmls"]
```

## Building

```sh
cargo build --release
```

tree-sitter-rpmspec comes from [a fork](https://gitlab.com/gleh/tree-sitter-rpmspec/-/tree/rpmls)
with the fixes of its grammar rpmls relies on.

A static binary, to run in containers of any distribution:

```sh
CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --target x86_64-unknown-linux-musl
```

At run time, rpmls needs `rpm`, for the macro files, and the linter: `rpmcrab`
or `rpmlint`.

rpmdevtools has an `rpmls` command too, listing the files of packages: put
this one first in the `PATH`, or give the editor its full path.
