# Tests for the comment-ban ast-grep rules: fixtures per language, header limits, justification words and suppressions.
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

PACK = Path(__file__).resolve().parent.parent
CONFIG = PACK / "sgconfig.yml"
FIXTURES = Path(__file__).resolve().parent / "fixtures"

NC = "no-comments"
SUP = "no-inline-suppressions"
JUST = "no-justification-header"
AST_GREP_MISSING = ("ast-grep is not on PATH. Install it with `uv tool install ast-grep-cli==0.51.1` "
                    "or `pip install ast-grep-cli==0.51.1`, then rerun the tests.")


def setUpModule():
    if shutil.which("ast-grep") is None:
        raise RuntimeError(AST_GREP_MISSING)


def scan_matches(files):
    with tempfile.TemporaryDirectory() as tmp:
        for name, text in files.items():
            path = Path(tmp, name)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8", newline="\n")
        proc = subprocess.run(
            [shutil.which("ast-grep"), "scan", "-c", str(CONFIG), "--json=compact", "."],
            cwd=tmp,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )
    if proc.returncode not in (0, 1):
        raise AssertionError(proc.stderr)
    return json.loads(proc.stdout)


def scan(files):
    found = set()
    for match in scan_matches(files):
        rule = match["ruleId"].rsplit("-", 1)[0]
        found.add((match["file"].replace("\\", "/").removeprefix("./"), match["range"]["start"]["line"] + 1, rule))
    return found


def expect(name, lines):
    return {(name, line, rule) for line, rules in lines.items() for rule in rules}


class Fixtures(unittest.TestCase):
    def check(self, fixture, name, lines):
        text = (FIXTURES / fixture).read_text(encoding="utf-8")
        self.assertEqual(scan({name: text}), expect(name, lines))

    def test_rust(self):
        self.check("rust.rs.txt", "sample.rs", {
            5: [NC], 7: [NC], 10: [NC], 21: [SUP], 24: [SUP], 28: [SUP], 35: [NC], 36: [NC], 41: [SUP],
        })

    def test_python(self):
        self.check("python.py.txt", "sample.py", {
            4: [NC], 15: [NC], 16: [NC], 20: [NC], 21: [NC], 25: [NC],
            30: [NC, SUP], 31: [NC, SUP], 32: [NC, SUP], 36: [NC],
        })

    def test_typescript(self):
        self.check("typescript.ts.txt", "sample.ts", {
            8: [NC], 10: [NC], 13: [NC, SUP], 15: [NC, SUP], 17: [NC, SUP], 19: [NC, SUP], 21: [NC, SUP],
        })

    def test_tsx(self):
        self.check("tsx.tsx.txt", "sample.tsx", {1: [SUP], 7: [NC]})

    def test_javascript(self):
        self.check("javascript.js.txt", "sample.js", {7: [NC], 8: [NC, SUP]})


HEADERS = {
    "a.rs": ("//", "fn main() {}\n"),
    "a.py": ("#", "x = 1\n"),
    "a.ts": ("//", "export const x = 1;\n"),
    "a.tsx": ("//", "export const x = <div />;\n"),
    "a.js": ("//", "module.exports = 1;\n"),
}


def with_header(prefix, lines, code):
    return "".join(f"{prefix} {line}\n" for line in lines) + code


class Headers(unittest.TestCase):
    def test_three_line_header_passes(self):
        for name, (prefix, code) in HEADERS.items():
            with self.subTest(name):
                self.assertEqual(scan({name: with_header(prefix, ["Parses orders.", "Second line.", "Third line."], code)}), set())

    def test_four_line_header_fails_on_fourth_line(self):
        for name, (prefix, code) in HEADERS.items():
            with self.subTest(name):
                text = with_header(prefix, ["Parses orders.", "Two.", "Three.", "Four."], code)
                self.assertEqual(scan({name: text}), {(name, 4, NC)})

    def test_header_after_shebang_passes(self):
        files = {
            "a.py": "#!/usr/bin/env python3\n# One.\n# Two.\n# Three.\nx = 1\n",
            "a.js": "#!/usr/bin/env node\n// One.\n// Two.\n// Three.\nmodule.exports = 1;\n",
        }
        self.assertEqual(scan(files), set())

    def test_block_header(self):
        self.assertEqual(scan({"a.ts": "/* One.\n   Two.\n   Three. */\nexport const x = 1;\n"}), set())
        self.assertEqual(scan({"a.ts": "/* One.\n   Two.\n   Three.\n   Four. */\nexport const x = 1;\n"}), {("a.ts", 1, NC)})
        self.assertEqual(scan({"a.ts": "/* One.\n   Two. */\n// Three.\n// Four.\nexport const x = 1;\n"}), {("a.ts", 1, NC)})

    def test_rust_doc_header_and_item_doc(self):
        self.assertEqual(scan({"a.rs": "//! One.\n//! Two.\n//! Three.\nfn main() {}\n"}), set())
        self.assertEqual(scan({"a.rs": "/// Item doc.\nfn main() {}\n"}), {("a.rs", 1, NC)})

    def test_comment_after_code_is_not_header(self):
        self.assertEqual(scan({"a.py": "x = 1\n# Not a header.\n"}), {("a.py", 2, NC)})


class Justification(unittest.TestCase):
    def test_justification_words_fail(self):
        words = ["shortcut", "short-cut", "Workaround", "work around", "works around", "stopgap", "hack",
                 "For Now", "TEMPORARY", "temporarily", "deliberately", "intentionally", "on purpose",
                 "TODO", "FIXME", "XXX", "the owner"]
        for word in words:
            for name, (prefix, code) in HEADERS.items():
                with self.subTest(word=word, file=name):
                    text = with_header(prefix, [f"Parses orders, {word} the old parser."], code)
                    self.assertEqual(scan({name: text}), {(name, 1, JUST)})

    def test_plain_header_passes(self):
        self.assertEqual(scan({"a.py": "# Parses orders from the exchange feed.\nx = 1\n"}), set())
        self.assertEqual(scan({"a.py": "# Order parser: reads the exchange feed, skips blank rows.\nx = 1\n"}), set())
        self.assertEqual(scan({"a.ts": "// Order parser: reads the exchange feed.\nexport const x = 1;\n"}), set())

    def test_temp_and_instead_of_pass(self):
        for name, (prefix, code) in HEADERS.items():
            with self.subTest(name):
                self.assertEqual(scan({name: with_header(prefix, ["Reads temp files instead of the cache."], code)}), set())

    def test_header_line_over_120_characters_fails(self):
        for name, (prefix, code) in HEADERS.items():
            with self.subTest(name):
                fits = prefix + " " + "x" * (119 - len(prefix))
                self.assertEqual(scan({name: f"{fits}\n{code}"}), set())
                self.assertEqual(scan({name: f"{fits}y\n{code}"}), {(name, 1, NC)})
        block = "/* Parses orders.\n   " + "x" * 118 + " */\nexport const x = 1;\n"
        self.assertEqual(scan({"a.ts": block}), {("a.ts", 1, NC)})


class Directives(unittest.TestCase):
    CASES = {
        "a.py": ["# pylint: disable=all", "# fmt: off", "# mypy: ignore-errors", "# pyright: basic", "# pragma: no cover",
                 "# isort: skip_file", "# flake8: noqa", "# Parses orders. pylint: disable=all", "# -*- coding: latin-1 -*-",
                 "# coding=latin-1", "# vim: set fileencoding=latin-1 :", "# nosec", "# type: ignore", "# nosemgrep",
                 "# noinspection PyUnresolvedReferences", "# lgtm[py/unused-import]", "# Parses orders. NOSONAR"],
        "a.js": ["/* istanbul ignore file */", "// prettier-ignore", "/* global foo */", "// oxlint-disable", "/* tslint:disable */",
                 "// deno-lint-ignore-file", "/* c8 ignore next */", "/* v8 ignore start */", "/* eslint-env node */", "// @flow",
                 "/* Parses orders.\n   global foo */", "//# sourceMappingURL=app.js.map", "//@ sourceMappingURL=app.js.map",
                 "// NOSONAR", "// noinspection JSUnresolvedFunction", "// lgtm[js/unused-local-variable]"],
        "a.ts": ["/// <reference types=\"node\" />", "// @ts-nocheck", "/* exported foo */", "// jscpd:ignore-start",
                 "//# sourceMappingURL=a.js.map", "// nosemgrep"],
        "a.tsx": ["/* istanbul ignore file */", "// prettier-ignore"],
        "a.rs": ["// @generated", "//! rustfmt: skip", "// NOSONAR", "// ignore-tidy-linelength"],
    }

    def test_directives_fail_even_as_headers(self):
        for name, lines in self.CASES.items():
            code = HEADERS[name][1]
            for line in lines:
                with self.subTest(file=name, line=line):
                    self.assertIn((name, 1, SUP), scan({name: f"{line}\n{code}"}))

    def test_utf8_cookie_and_shebang_pass(self):
        for text in ["#!/usr/bin/env python3\n# -*- coding: utf-8 -*-\n# Parses orders.\nx = 1\n",
                     "# coding: utf-8\nx = 1\n", "# coding=UTF8\nx = 1\n", "#!/usr/bin/env python3\nx = 1\n"]:
            with self.subTest(text):
                self.assertEqual(scan({"a.py": text}), set())

    def test_cookie_with_extra_text_fails(self):
        self.assertIn(("a.py", 1, SUP), scan({"a.py": "# coding: utf-8 pylint: disable=all\nx = 1\n"}))

    def test_directive_word_inside_a_url_fails(self):
        lines = ["Parses orders, see https://eslint.org/docs/rules/no-ignore", "Format at http://example.com/pragma/noqa-disable",
                 "Parses settings files, see https://example.com/pylint:disable=all", "Parses settings, see https://example.com/#noqa",
                 "Parses settings, see https://example.com/#nosec"]
        for name, (prefix, code) in HEADERS.items():
            for line in lines:
                with self.subTest(file=name, line=line):
                    self.assertEqual(scan({name: with_header(prefix, [line], code)}), {(name, 1, SUP)})

    def test_plain_url_passes(self):
        for name, (prefix, code) in HEADERS.items():
            with self.subTest(name):
                self.assertEqual(scan({name: with_header(prefix, ["Parses orders, see https://example.com/orders for the spec."], code)}), set())

    def test_url_does_not_hide_the_first_word_or_a_later_directive(self):
        for name, (prefix, code) in HEADERS.items():
            for line in ["Spec: see https://example.com/orders", "https://example.com/orders", "See https://example.com noqa"]:
                with self.subTest(file=name, line=line):
                    self.assertEqual(scan({name: with_header(prefix, [line], code)}), {(name, 1, SUP)})

    def test_word_colon_message_says_to_rephrase(self):
        for name, (prefix, code) in HEADERS.items():
            with self.subTest(name):
                messages = [m["message"] for m in scan_matches({name: with_header(prefix, ["Spec: orders."], code)})]
                self.assertEqual(len(messages), 1)
                self.assertIn("starts with one word and a colon reads as a directive, so rephrase it", messages[0])


NEXT_ENV = ('/// <reference types="next" />\n/// <reference types="next/image-types/global" />\n\n'
            "// NOTE: This file should not be edited\n// see https://nextjs.org/docs/basic-features/typescript for more information.\n")


class NextEnv(unittest.TestCase):
    def test_generated_next_env_passes(self):
        self.assertEqual(scan({"next-env.d.ts": NEXT_ENV, "apps/web/next-env.d.ts": NEXT_ENV}), set())

    def test_reference_in_other_source_fails_and_points_at_tsconfig(self):
        matches = scan_matches({"env.d.ts": '/// <reference types="next" />\nexport {};\n'})
        self.assertEqual({(m["ruleId"], m["range"]["start"]["line"] + 1) for m in matches}, {("no-inline-suppressions-ts", 1)})
        self.assertIn("compilerOptions.types in tsconfig.json", matches[0]["message"])


class HtmlComments(unittest.TestCase):
    def test_html_comments_fail(self):
        for name in ["a.js", "a.ts", "a.tsx"]:
            with self.subTest(name):
                text = "export const x = 1;\n<!-- hidden\n--> also hidden\n"
                self.assertEqual(scan({name: text}), {(name, 2, NC), (name, 3, NC)})
                self.assertEqual(scan({name: "<!-- Parses orders.\nexport const x = 1;\n"}), {(name, 1, NC)})
                self.assertIn((name, 2, SUP), scan({name: "export const x = 1;\n<!-- @ts-ignore\n"}))


class BareStrings(unittest.TestCase):
    def test_python_bare_strings_fail_anywhere(self):
        text = 'x = 1\n"bare"\n(("parenthesized"))\ndef f():\n    y = 2\n    "later"\n    if y:\n        f"fmt"\n    "a" "b"\n'
        self.assertEqual(scan({"a.py": text}), {("a.py", line, NC) for line in [2, 3, 6, 8, 9]})

    def test_python_strings_in_expressions_pass(self):
        self.assertEqual(scan({"a.py": 'x = "a"\ny = ("b")\nprint("c")\nz = "d" "e"\n'}), set())

    def test_js_bare_strings_fail(self):
        text = 'export const x = 1;\n"comment";\n`template`;\n("paren");\n"use strict";\nif (x) { "use strict"; }\n{ "use client"; }\n'
        for name in ["a.js", "a.ts", "a.tsx"]:
            with self.subTest(name):
                self.assertEqual(scan({name: text}), {(name, line, NC) for line in range(2, 8)})

    def test_directive_prologues_pass(self):
        files = {
            "a.js": '#!/usr/bin/env node\n"use strict";\nmodule.exports = 1;\n',
            "b.ts": '// Server actions.\n"use server";\n\'use strict\';\nexport async function f() { "use server"; return 1; }\n',
            "c.tsx": '"use client";\nexport const g = () => { "use strict"; return <div />; };\n',
            "d.js": 'class C { m() { "use strict"; } }\nconst h = function () { "use strict"; };\nfunction* k() { "use strict"; }\n',
        }
        self.assertEqual(scan(files), set())

    def test_late_directive_fails(self):
        self.assertEqual(scan({"a.js": 'function f() { const z = 1; "use strict"; }\n'}), {("a.js", 1, NC)})

    def test_rust_bare_strings_fail(self):
        self.assertEqual(scan({"a.rs": 'fn f() -> &\'static str {\n    "comment";\n    r"raw";\n    "tail"\n}\n'}),
                         {("a.rs", 2, NC), ("a.rs", 3, NC)})

    def test_python_string_assigned_to_underscore_fails(self):
        text = 'x = 1\n_ = "comment"\n_: str = ("typed")\n_ = "a" "b"\n_ = f("call")\n_x = "named"\n_: "T" = 1\ny = "kept"\n'
        self.assertEqual(scan({"a.py": text}), {("a.py", line, NC) for line in [2, 3, 4]})

    def test_js_void_string_fails(self):
        text = 'export const x = 1;\nvoid "comment";\nconst a = void ("paren");\nvoid `template`;\nvoid 0;\nconst b = void x;\n'
        for name in ["a.js", "a.ts", "a.tsx"]:
            with self.subTest(name):
                self.assertEqual(scan({name: text}), {(name, line, NC) for line in [2, 3, 4]})

    def test_rust_doc_attribute_fails(self):
        text = ('#![doc = r"inner"]\n#[doc = "outer"]\nfn f() {}\n#[cfg_attr(docsrs, doc = "conditional")]\nfn g() {}\n'
                '#[doc(hidden)]\nfn h() {}\n#[doc(alias = "q")]\nfn k() {}\n')
        self.assertEqual(scan({"a.rs": text}), {("a.rs", line, NC) for line in [1, 2, 4]})


class RustSafety(unittest.TestCase):
    def test_safety_comment_before_unsafe_code_passes(self):
        text = ("fn f() -> u8 {\n    // SAFETY: g has no preconditions.\n    unsafe { g() };\n"
                "    // SAFETY: g has no preconditions.\n    let x = unsafe { g() };\n"
                "    // SAFETY: g has no preconditions.\n    unsafe { g() }\n}\n"
                "// SAFETY: X holds no thread-bound state.\nunsafe impl Send for X {}\n")
        self.assertEqual(scan({"a.rs": text}), set())

    def test_safety_comment_elsewhere_fails(self):
        text = ("fn f() {\n    // SAFETY: not before unsafe code.\n    let y = 1;\n"
                "    // SAFETY: a second comment line sits between.\n    // g has no preconditions.\n    unsafe { g() };\n}\n")
        self.assertEqual(scan({"a.rs": text}), {("a.rs", 2, NC), ("a.rs", 2, SUP), ("a.rs", 4, NC), ("a.rs", 4, SUP), ("a.rs", 5, NC)})

    def test_safety_header_fails_as_directive(self):
        self.assertEqual(scan({"a.rs": "// SAFETY: header line.\nfn main() {}\n"}), {("a.rs", 1, SUP)})

    def test_safety_comment_still_checked_for_directives(self):
        text = "fn f() {\n    // SAFETY: fine. ignore-tidy-linelength\n    unsafe { g() };\n}\n"
        self.assertEqual(scan({"a.rs": text}), {("a.rs", 2, SUP)})


class Suppressions(unittest.TestCase):
    def test_each_form_fails(self):
        cases = {
            "a.ts": ["// eslint-disable-next-line", "/* eslint-disable */", "// @ts-ignore", "// @ts-expect-error",
                     "// @ts-nocheck", "// biome-ignore lint: x"],
            "a.tsx": ["// eslint-disable-line", "export const y = <div>{/* @ts-ignore */}</div>;"],
            "a.js": ["// eslint-disable-line no-undef", "/* biome-ignore format: x */"],
            "a.py": ["# noqa", "# noqa: E501", "# type: ignore", "# pyright: ignore[reportGeneralTypeIssues]", "# ruff: noqa"],
        }
        for name, lines in cases.items():
            first = "x = 1\n" if name == "a.py" else "export const x = 1;\n"
            for line in lines:
                with self.subTest(file=name, line=line):
                    self.assertIn((name, 2, SUP), scan({name: f"{first}{line}\n"}))

    def test_rust_attributes_fail(self):
        for attribute in ["#[allow(dead_code)]", "#[expect(unused)]", "#![allow(clippy::all)]"]:
            with self.subTest(attribute):
                self.assertEqual(scan({"a.rs": f"{attribute}\nfn f() {{}}\n"}), {("a.rs", 1, SUP)})

    def test_rust_cfg_attr_and_rustfmt_skip_fail(self):
        for attribute in ["#[cfg_attr(test, allow(dead_code))]", "#![cfg_attr(feature = \"x\", expect(unused))]",
                          "#[cfg_attr(a, cfg_attr(b, allow(x)))]", "#[cfg_attr(test, derive(Debug), allow(x))]",
                          "#[rustfmt::skip]", "#![rustfmt::skip]", "#[rustfmt::skip::macros(vec)]",
                          "#[cfg_attr(rustfmt, rustfmt::skip)]"]:
            with self.subTest(attribute):
                self.assertEqual(scan({"a.rs": f"{attribute}\nfn f() {{}}\n"}), {("a.rs", 1, SUP)})

    def test_rust_attributes_in_macro_token_trees_fail(self):
        for attribute in ["#[allow(dead_code)]", "#[expect(unused)]", "#![allow(clippy::all)]", "# [ allow ( x ) ]",
                          "#[cfg_attr(test, allow(x))]", "#[rustfmt::skip]"]:
            for text in [f"macro_rules! m {{\n    () => {{\n        {attribute}\n        fn f() {{}}\n    }};\n}}\n",
                         f"fn g() {{}}\nm! {{\n    {attribute}\n    fn f() {{}}\n}}\n"]:
                with self.subTest(attribute=attribute, text=text):
                    self.assertEqual(scan({"a.rs": text}), {("a.rs", 3, SUP)})

    def test_rust_macro_token_trees_without_suppressions_pass(self):
        text = ("macro_rules! m {\n    () => {\n        #[derive(Debug)]\n        #[cfg(test)]\n        struct S;\n"
                "        let v = a[allow(1)];\n        n![allow(x)];\n        #[cfg_attr(test, derive(Clone))]\n        struct T;\n    };\n}\n")
        self.assertEqual(scan({"a.rs": text}), set())

    def test_rust_other_attributes_pass(self):
        self.assertEqual(scan({"a.rs": "#[derive(Debug)]\n#[cfg(test)]\n#[cfg_attr(test, derive(Clone))]\nstruct S;\n"}), set())

    def test_suppressed_words_in_strings_pass(self):
        files = {
            "a.py": 'x = "# noqa"\ny = "type: ignore"\n',
            "a.ts": 'export const x = "// eslint-disable-line @ts-ignore";\n',
        }
        self.assertEqual(scan(files), set())


class MissingAstGrep(unittest.TestCase):
    def test_suite_fails_once_with_the_install_command(self):
        env = dict(os.environ, PATH=str(Path(shutil.which("git")).parent))
        proc = subprocess.run([sys.executable, "-m", "unittest", "test_rules.Headers"], cwd=Path(__file__).parent,
                              capture_output=True, text=True, encoding="utf-8", env=env)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn(AST_GREP_MISSING, proc.stderr)
        self.assertNotIn("TypeError", proc.stderr)
        self.assertEqual(proc.stderr.count("ERROR: "), 1, proc.stderr)


if __name__ == "__main__":
    unittest.main()
