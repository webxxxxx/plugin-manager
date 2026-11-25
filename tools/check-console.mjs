//! Contract checks for the embedded console page (`src/assets/static/index.html`).
//!
//! There is no browser or DOM library in this repo, so the settings sheet's
//! structure is checked at the source level: the tab strip must exist, every
//! group must become its own page, and switching a tab must only toggle classes
//! (#297). Rebuilding on switch would drop unsaved edits, which is exactly the
//! failure those last two checks guard.
//!
//! `--self-test` mutates a copy of the page once per check and proves the check
//! goes red, then runs the real page and proves it comes back green. Any new
//! check must add its own mutation there.
import { readFileSync } from "node:fs";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import vm from "node:vm";

const DEFAULT_PAGE = "src/assets/static/index.html";

function readPage(path) {
  // The checkout is CRLF on Windows; every check here is a substring match, so
  // one line ending keeps the multi-line ones working on both hosts.
  return readFileSync(path, "utf8").replace(/\r\n/g, "\n");
}

function scriptOf(page) {
  const match = /<script[^>]*>([\s\S]*?)<\/script>/.exec(page);
  if (!match) throw new Error("the page has no inline script");
  return match[1];
}

/** The source of `function name(...) { ... }` including its body braces. */
function functionBody(page, name) {
  const start = page.indexOf("function " + name + "(");
  if (start < 0) throw new Error("the page has no function " + name);
  const open = page.indexOf("{", start);
  let depth = 0;
  for (let index = open; index < page.length; index += 1) {
    const char = page[index];
    if (char === "{") depth += 1;
    else if (char === "}") {
      depth -= 1;
      if (depth === 0) return page.slice(open + 1, index);
    }
  }
  throw new Error("function " + name + " is not closed");
}

function cssRule(page, selector) {
  const start = page.indexOf(selector + " {");
  if (start < 0) return null;
  const end = page.indexOf("}", start);
  return page.slice(start + selector.length + 1, end);
}

const CHECKS = [
  {
    id: "script-compiles",
    run(page) {
      new vm.Script(scriptOf(page));
      return "the inline script compiles";
    },
    mutate(page) {
      return page.replace("<script>", "<script>\nfunction (");
    },
  },
  {
    id: "tab-page-hidden",
    run(page) {
      const hidden = cssRule(page, ".settings-tab");
      const shown = cssRule(page, ".settings-tab.active");
      if (!hidden || !/display:\s*none/.test(hidden)) {
        throw new Error("a settings page must be hidden until it is active");
      }
      if (!shown || !/display:\s*block/.test(shown)) {
        throw new Error("the active settings page must be shown");
      }
      return "only the active page is displayed";
    },
    mutate(page) {
      return page.replace(".settings-tab { display:none; }", ".settings-tab { display:block; }");
    },
  },
  {
    id: "tab-strip-scrolls",
    run(page) {
      const strip = cssRule(page, ".sheet-tabs");
      if (!strip || !/overflow-x:\s*auto/.test(strip)) {
        throw new Error("the tab strip must scroll sideways on a narrow screen");
      }
      return "the tab strip scrolls horizontally";
    },
    mutate(page) {
      return page.replace(/(\.sheet-tabs \{[^}]*?)overflow-x:auto;/s, "$1overflow-x:hidden;");
    },
  },
  {
    id: "panel-order",
    run(page) {
      const sheet = functionBody(page, "showSettingsModal");
      const order = ["head", "tabs", "body", "actions"].map((part) => {
        const at = sheet.indexOf("panel.appendChild(" + part + ");");
        if (at < 0) throw new Error("the sheet never appends " + part);
        return at;
      });
      const sorted = [...order].sort((left, right) => left - right);
      if (order.join() !== sorted.join()) {
        throw new Error("the sheet must append head, tabs, body, actions in that order");
      }
      return "the tab strip sits between the head and the body";
    },
    mutate(page) {
      return page.replace(
        "panel.appendChild(tabs);\n  panel.appendChild(body);",
        "panel.appendChild(body);\n  panel.appendChild(tabs);",
      );
    },
  },
  {
    id: "pages-built-once",
    run(page) {
      const body = functionBody(page, "showTab");
      for (const rebuild of ["field(", "passwordField(", "innerHTML", "replaceChildren", "appendChild("]) {
        if (body.includes(rebuild)) {
          throw new Error("showTab rebuilds the form (" + rebuild + "): switching would drop edits");
        }
      }
      if (!body.includes("classList.toggle(")) {
        throw new Error("showTab must only toggle the active class");
      }
      return "switching a tab only toggles classes";
    },
    mutate(page) {
      return page.replace(
        "hideSettingsTip();\n  }\n\n  // A dot on every tab",
        "hideSettingsTip();\n    body.appendChild(document.createElement('div'));\n  }\n\n  // A dot on every tab",
      );
    },
  },
  {
    id: "pages-registered",
    run(page) {
      const loop = functionBody(page, "showSettingsModal");
      for (const binding of ["tabButtons[group] = tab;", "pages[group] = page;"]) {
        if (!loop.includes(binding)) throw new Error("a group is not registered: " + binding);
      }
      if (!loop.includes("showTab(groups[0])")) {
        throw new Error("the sheet must open on the first group");
      }
      return "every group gets a tab and a page";
    },
    mutate(page) {
      return page.replace("    pages[group] = page;\n", "");
    },
  },
  {
    id: "pending-edits-marked",
    run(page) {
      if (!functionBody(page, "mark").includes("markTabs()")) {
        throw new Error("mark() must flag the tab that owns the edit");
      }
      const tabs = functionBody(page, "markTabs");
      if (!tabs.includes("hasOwnProperty.call(changes")) {
        throw new Error("markTabs must look at the pending changes");
      }
      return "a tab with an unsaved edit is marked";
    },
    mutate(page) {
      return page.replace("changes[key] = value;\n    markTabs();", "changes[key] = value;");
    },
  },
  {
    id: "roots-locked",
    run(page) {
      const body = functionBody(page, "pluginRootsField");
      if (!body.includes("root.default || plugins > 0")) {
        throw new Error("the default root and roots with plugins must be locked");
      }
      if (!body.includes("input.readOnly = locked") || !body.includes("remove.disabled = locked")) {
        throw new Error("a locked root must be uneditable and undeletable");
      }
      return "the default root and every occupied root are locked";
    },
    mutate(page) {
      return page.replace(
        "input.value = path;\n    input.readOnly = locked;",
        "input.value = path;\n    input.readOnly = false;",
      );
    },
  },
  {
    id: "roots-save-one-line",
    run(page) {
      const body = functionBody(page, "pluginRootsField");
      if (!body.includes("if (value) paths.push(value);")) {
        throw new Error("an empty row must not become an empty list entry");
      }
      if (!body.includes("paths.join(',')") || !body.includes("mark(entry.key, joined)")) {
        throw new Error("the roots must save as one comma-separated line");
      }
      return "the roots save as one comma-separated line";
    },
    mutate(page) {
      return page.replace("if (value) paths.push(value);", "paths.push(value);");
    },
  },
  {
    id: "roots-revert-unmarks",
    run(page) {
      const body = functionBody(page, "pluginRootsField");
      if (!body.includes("if (joined === initial) unmark(entry.key);")) {
        throw new Error("a list that ends where it started must not be saved");
      }
      return "returning to the starting list drops the pending change";
    },
    mutate(page) {
      return page.replace(
        "if (joined === initial) unmark(entry.key);\n    else mark(entry.key, joined);",
        "mark(entry.key, joined);",
      );
    },
  },
  {
    id: "roots-editor-wired",
    run(page) {
      if (!page.includes("entry.key === 'plugins_dir'")) {
        throw new Error("the plugins_dir row is not bound to the list editor");
      }
      return "plugins_dir renders the list editor";
    },
    mutate(page) {
      return page.replace("entry.key === 'plugins_dir'", "entry.key === 'plugins_dir_old'");
    },
  },
  {
    id: "install-root-asked-when-enabled",
    run(page) {
      const body = functionBody(page, "install");
      if (!body.includes("disk.manualInstallRoot") || !body.includes("roots.length < 2")) {
        throw new Error("the picker must depend on the switch and on more than one root");
      }
      if (!body.includes("chooseInstallRoot(file, roots)")) {
        throw new Error("install must ask for a root when the switch is on");
      }
      return "the picker only appears when the switch is on and roots > 1";
    },
    mutate(page) {
      return page.replace(
        "if (!disk.manualInstallRoot || roots.length < 2) {",
        "if (roots.length < 2) {",
      );
    },
  },
  {
    id: "install-root-passed-explicitly",
    run(page) {
      const body = functionBody(page, "uploadInstall");
      if (!body.includes("'?root=' + encodeURIComponent(root)")) {
        throw new Error("an explicit root must reach the install endpoint");
      }
      return "the chosen root travels as ?root=";
    },
    mutate(page) {
      return page.replace(
        "'/api/v1/plugins' + (root ? '?root=' + encodeURIComponent(root) : '')",
        "'/api/v1/plugins'",
      );
    },
  },
  {
    id: "no-flat-list",
    run(page) {
      if (page.includes("heading.className = 'setting-group'")) {
        throw new Error("the sheet still renders one flat list of groups");
      }
      return "the flat group list is gone";
    },
    mutate(page) {
      return page.replace(
        "<script>",
        "<script>\n// heading.className = 'setting-group'",
      );
    },
  },
];

function runAll(page) {
  const failures = [];
  for (const check of CHECKS) {
    try {
      check.run(page);
    } catch (error) {
      failures.push(check.id + ": " + error.message);
    }
  }
  return failures;
}

function selfTest(page) {
  let failed = 0;
  for (const check of CHECKS) {
    const broken = check.mutate(page);
    if (broken === page) {
      console.error("selftest: " + check.id + " has no mutation that changes the page");
      failed += 1;
      continue;
    }
    const caught = runAll(broken).some((line) => line.startsWith(check.id + ":"));
    if (!caught) {
      console.error("selftest: " + check.id + " stays green on a broken page");
      failed += 1;
    }
  }
  return failed;
}

const pagePath = process.env.SVCD_CONSOLE_PAGE || DEFAULT_PAGE;
let page;
try {
  page = readPage(pagePath);
} catch (error) {
  console.error("console: cannot read " + pagePath + ": " + error.message);
  process.exit(2);
}

if (process.argv.includes("--self-test")) {
  const failures = selfTest(page);
  if (failures > 0) {
    console.log("console: selftest FAILED (" + failures + " check(s) did not catch their mutation)");
    process.exit(1);
  }
  console.log("console: selftest ok (" + CHECKS.length + " checks, every mutation caught)");
}

const failures = runAll(page);
for (const failure of failures) console.log("console: " + failure);
if (failures.length > 0) {
  console.log("console: FAILED (" + failures.length + " violation(s))");
  process.exit(1);
}
console.log("console: ok (" + CHECKS.length + " checks)");
