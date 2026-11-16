# Tests for trading-desk-layers.yml: each import and directory rule fires on a bad sample and not on a good one.
import unittest
from pathlib import Path

from test_rules import scan_matches


def layer_hits(files):
    return {(m["file"].replace("\\", "/").removeprefix("./"), m["ruleId"])
            for m in scan_matches(files) if m["ruleId"].startswith("layers-")}


NODE_AND_ELECTRON = ["fs", "path", "os", "child_process", "http", "https", "net", "crypto", "url", "worker_threads",
                     "stream", "events", "fs/promises", "electron/main"]
NODE_AND_ELECTRON_IMPORTS = [f"import x from '{module}'\n" for module in NODE_AND_ELECTRON]

CASES = [
    ("layers-td-core-pure", "crates/td-core/src/x.rs",
     ["use td_engine::Strategy;\n", "use std::fs;\n", "use std::{io, net};\n", "use std::{fs::File, io};\n",
      "use std::{process::{self, Command}};\n", "fn f() { std::process::exit(1) }\n",
      "fn f() -> tokio::runtime::Runtime { todo!() }\n"],
     ["use serde::Serialize;\nuse std::{fmt, io};\n", "fn f() { let fs = 1; }\n",
      "use std::{collections::HashMap};\n"]),
    ("layers-td-engine-no-venue", "crates/td-engine/src/x.rs",
     ["use td_data::Store;\n", "use std::net::TcpStream;\n", "use std::{net::TcpStream};\n", "use std::{net};\n",
      "fn f() { reqwest::get(\"\"); }\n", "fn f() { ::tokio::spawn(x); }\n", "use tokio::*;\n",
      "extern crate tokio;\n", "use {rayon, tokio as rt};\n"],
     ["use td_core::Candle;\nuse std::fs;\nuse rayon::prelude::*;\n", "use std::{collections::HashMap};\n",
      "fn f() { crate::util::tokio::x(); }\n", "use crate::util::{tokio, td_data::Store};\n"]),
    ("layers-td-data-adapter", "crates/td-data/src/x.rs",
     ["use td_engine::Strategy;\n", "use td_orders::Client;\n"],
     ["use td_core::Candle;\nuse reqwest::Url;\n"]),
    ("layers-td-orders-adapter", "crates/td-orders/src/x.rs",
     ["use td_data::Store;\n", "use td_live::VenueBroker;\n"],
     ["use td_core::Side;\nuse td_mock_bybit::MockVenue;\n"]),
    ("layers-td-server-sends-no-orders", "crates/td-server/src/x.rs",
     ["use td_orders::Client;\n", "fn f() { td_live::run(); }\n", "use td_mock_bybit::MockVenue;\n"],
     ["use td_engine::Strategy;\nuse td_data::Store;\n"]),
    ("layers-mock-bybit-independent", "mock/bybit/src/x.rs",
     ["use td_orders::wire::Order;\n"],
     ["use td_core::Side;\n"]),
    ("layers-rust-fixtures-through-mock", "crates/td-data/src/x.rs",
     ['const A: &str = include_str!("../../../gui/app/fixtures/a.json");\n'],
     ['const A: &str = include_str!("../../../mock/bybit/fixtures/a.json");\n'
      'const B: &str = include_str!("../tests/fixtures/b.json");\n']),
    ("layers-gui-shared-stands-alone", "gui/shared/src/x.ts",
     ["import { app } from 'electron'\n", "import fs from 'node:fs'\n", "import { useState } from 'react'\n",
      "export { x } from '../../app/src/main/engine'\n", *NODE_AND_ELECTRON_IMPORTS],
     ["import type { Run } from './api-types'\nexport * from './client'\n"]),
    ("layers-gui-renderer-sandboxed-ts", "gui/app/src/renderer/src/x.ts",
     ["import { ipcRenderer } from 'electron'\n", "import { readFile } from 'node:fs/promises'\n",
      "import { start } from '../../main/engine'\n", "const m = await import('../../preload/index')\n",
      *NODE_AND_ELECTRON_IMPORTS],
     ["import { client } from '@td/shared'\nimport { fmt } from './lab/format'\nimport { main } from './main'\n",
      "import { toUrl } from './url'\nimport { events } from './trading/events'\n"]),
    ("layers-gui-renderer-sandboxed-tsx", "gui/app/src/renderer/src/X.tsx",
     ["import path from 'path'\nexport const X = () => <div />\n", *NODE_AND_ELECTRON_IMPORTS],
     ["import { useState } from 'react'\nexport const X = () => <div />\n"]),
    ("layers-gui-main-no-renderer-ts", "gui/app/src/main/x.ts",
     ["import { fmt } from '../renderer/src/lab/format'\n", "import { useState } from 'react'\n"],
     ["import { app } from 'electron'\nimport { spawn } from 'node:child_process'\nimport { client } from '@td/shared'\n"]),
    ("layers-gui-main-no-renderer-tsx", "gui/app/src/preload/X.tsx",
     ["import { Chart } from '../renderer/src/trading/Chart'\nexport const X = () => <div />\n"],
     ["import { client } from '@td/shared'\nexport const X = () => <div />\n"]),
    ("layers-gui-mock-through-source-ts", "gui/app/src/renderer/src/lab/runs.ts",
     ["import { lab } from '../../../../mock/lab'\n", "import engine from '../../../../mock/engine.json'\n"],
     ["import { source } from './source'\nimport { mockup } from './mockup'\n"]),
    ("layers-gui-mock-through-source-tsx", "gui/app/src/renderer/src/lab/X.tsx",
     ["import { lab } from '../../../../mock/lab'\nexport const X = () => <div />\n"],
     ["import { source } from './source'\nexport const X = () => <div />\n"]),
    ("layers-gui-app-no-harness-ts", "gui/app/src/main/x.ts",
     ["import { serve } from '../../shots/harness.mjs'\n", "import { audit } from '../../../e2e/audit'\n",
      "import { procs } from '../../../scripts/procs.mjs'\n", "import fake from '../../test/fake-sidecar.mjs'\n"],
     ["import { bridge } from './bridge'\n"]),
    ("layers-gui-app-no-harness-tsx", "gui/app/src/renderer/src/X.tsx",
     ["import { serve } from '../../../shots/harness.mjs'\nexport const X = () => <div />\n"],
     ["import { Chart } from './trading/Chart'\nexport const X = () => <div />\n"]),
]


class Layers(unittest.TestCase):
    def test_each_bad_sample_fires_its_rule(self):
        for rule, path, bad, _ in CASES:
            for text in bad:
                with self.subTest(rule=rule, text=text):
                    self.assertIn((path, rule), layer_hits({path: text}))

    def test_each_good_sample_passes(self):
        for rule, path, _, good in CASES:
            for text in good:
                with self.subTest(rule=rule, text=text):
                    self.assertEqual(layer_hits({path: text}), set())

    def test_rules_apply_only_to_their_folders(self):
        self.assertEqual(layer_hits({"crates/td-live/src/x.rs": "use td_orders::Client;\nuse td_engine::Strategy;\n"}), set())
        self.assertEqual(layer_hits({"crates/td-core/tests/x.rs": "use std::process::Command;\n"}), set())
        self.assertEqual(layer_hits({"crates/td-engine/examples/x.rs": "use td_data::Store;\n"}), set())
        self.assertEqual(layer_hits({"gui/app/src/renderer/src/lab/source.ts": "import { lab } from '../../../../mock/lab'\n"}), set())
        self.assertEqual(layer_hits({"gui/app/src/main/x.test.ts": "import fake from '../../test/fake-sidecar.mjs'\n"}), set())
        self.assertEqual(layer_hits({"gui/shared/src/x.test.ts": "import fs from 'node:fs'\n"}), set())

    def test_every_rule_has_a_case(self):
        text = (Path(__file__).resolve().parent.parent / "rules" / "trading-desk-layers.yml").read_text(encoding="utf-8")
        ids = {line.split(":", 1)[1].strip() for line in text.splitlines() if line.startswith("id:")}
        self.assertEqual(ids, {rule for rule, _, _, _ in CASES})


if __name__ == "__main__":
    unittest.main()
