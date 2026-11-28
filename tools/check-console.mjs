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
    id: "tab-strip-wraps",
    run(page) {
      const strip = cssRule(page, ".sheet-tabs");
      if (!strip || !/flex-wrap:\s*wrap/.test(strip)) {
        throw new Error("the tab strip must wrap instead of scrolling sideways");
      }
      if (/overflow-x:\s*(auto|scroll)/.test(strip)) {
        throw new Error("a sideways scrollbar hides tabs with no hint that they exist");
      }
      return "the tab strip wraps instead of scrolling sideways";
    },
    mutate(page) {
      return page.replace(/(\.sheet-tabs \{[^}]*?)flex-wrap:wrap;/s, "$1overflow-x:auto;");
    },
  },
  {
    id: "sheet-fixed-height",
    run(page) {
      const sheet = cssRule(page, ".modal .panel.sheet");
      if (!sheet || !/height:\s*min\(/.test(sheet)) {
        throw new Error("the settings sheet needs a fixed height: tabs hold different row counts");
      }
      const body = cssRule(page, ".sheet .sheet-body");
      if (!body || !/overflow:\s*auto/.test(body)) {
        throw new Error("only the body may scroll inside a fixed-height sheet");
      }
      return "the sheet keeps one height while the body scrolls";
    },
    mutate(page) {
      return page.replace("height:min(720px,90vh); ", "");
    },
  },
  {
    id: "no-readonly-badge",
    run(page) {
      if (page.includes("'只读'") || page.includes('"只读"')) {
        throw new Error("a read-only value is already styled; the badge is noise");
      }
      if (page.includes("setting-source")) {
        throw new Error("the read-only badge class must be gone");
      }
      return "read-only values carry no badge";
    },
    mutate(page) {
      return page.replace(
        "      control.appendChild(value);",
        "      control.appendChild(value);\n      control.appendChild(document.createTextNode('只读'));",
      );
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
    id: "roots-not-editable",
    run(page) {
      const body = functionBody(page, "pluginRootsField");
      if (/input\.readOnly/.test(body) || /item\.input/.test(body)) {
        throw new Error("a root must not be editable in place: re-typing an occupied path is a refused move");
      }
      if (!body.includes("root-row-path")) {
        throw new Error("a root row must show its path as text");
      }
      if (!body.includes("root.default") || !body.includes("plugins > 0")) {
        throw new Error("the default root and roots with plugins must not be removable");
      }
      if (!body.includes("remove.disabled = locked")) {
        throw new Error("a root that cannot be removed must disable its button");
      }
      return "roots are paths: added or removed, never rewritten";
    },
    mutate(page) {
      return page.replace(
        "const text = document.createElement('span');",
        "const input = document.createElement('input');\n    input.readOnly = false;\n    const text = input;",
      );
    },
  },
  {
    id: "roots-added-from-a-field",
    run(page) {
      const body = functionBody(page, "pluginRootsField");
      if (!body.includes("'+ 新增根'") || !body.includes("root-add")) {
        throw new Error("a new root must come from the add field, not from editing a row");
      }
      if (!body.includes("addRow(path, 0, false)")) {
        throw new Error("the add button must turn the typed path into a row");
      }
      return "a typed path becomes a row only when it is added";
    },
    mutate(page) {
      return page.replace("addRow(path, 0, false);", "");
    },
  },
  {
    id: "roots-save-one-line",
    run(page) {
      const body = functionBody(page, "pluginRootsField");
      if (!body.includes("const joined = rows.map(function (item) { return item.path; }).join(',')")) {
        throw new Error("the roots must save as one comma-separated line built from the rows");
      }
      if (!body.includes("mark(entry.key, joined)")) {
        throw new Error("a changed list must mark the key");
      }
      return "the roots save as one comma-separated line";
    },
    mutate(page) {
      return page.replace(
        "const joined = rows.map(function (item) { return item.path; }).join(',')",
        "const joined = rows.map(function (item) { return item.path; }).join(';')",
      );
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
    id: "plugin-list-shows-root-index",
    run(page) {
      const body = functionBody(page, "render");
      if (!body.includes("where.textContent = plugin.id;")) {
        throw new Error("the line under a plugin name is its id plus its root number");
      }
      if (!body.includes("badge.textContent = plugin.installRootIndex")) {
        throw new Error("the number must be the plugin root index");
      }
      if (!body.includes("where.title = '安装位置：' + plugin.installDir")) {
        throw new Error("the full install path belongs in the tip, not in the row");
      }
      // The path itself must never be printed: it is what made the row too wide.
      if (/textContent = [^;]*plugin\.installDir/.test(body)) {
        throw new Error("the install path must not be printed in the row");
      }
      return "the line under a plugin name is its id and its root number";
    },
    mutate(page) {
      return page.replace(
        "badge.textContent = plugin.installRootIndex ? String(plugin.installRootIndex) : '';",
        "badge.textContent = plugin.installDir;",
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
  {
    id: "login-submits-on-enter",
    run(page) {
      // The login card has one field and one button; a phone keyboard covers
      // the button, so the field itself has to submit on Enter.
      const handler =
        /\$\('pass'\)\.addEventListener\('keydown', function \(event\) \{([\s\S]*?)\n\}\);/.exec(page);
      if (!handler) {
        throw new Error("the password field only submits through the button");
      }
      if (!/event\.key [!=]== 'Enter'/.test(handler[1])) {
        throw new Error("the keydown handler must look at the Enter key");
      }
      if (!/preventDefault\(\)/.test(handler[1])) {
        throw new Error("Enter must not fall through to the default action");
      }
      if (!/login\(\);/.test(handler[1])) {
        throw new Error("Enter must call login(), the path the button takes");
      }
      return "the password field submits on Enter";
    },
    mutate(page) {
      return page.replace(
        "$('pass').addEventListener('keydown'",
        "$('pass').addEventListener('keyup'",
      );
    },
  },
  {
    id: "space-recovery-is-conservative",
    run(page) {
      const recover = new Function("data", functionBody(page, "spaceRecovery"));
      // Uninstalling returns `installed` to the volume; covering the whole
      // `need` with it is the test that holds whether or not the host counts
      // the old copy inside `need`.
      const fits = recover({ id: "demo", need: 100, installed: 40, roots: [{ root: "/r", free: 70 }] });
      if (!fits.canUninstall) {
        throw new Error("a refusal that uninstalling resolves must offer it");
      }
      const short = recover({ id: "demo", need: 100, installed: 10, roots: [{ root: "/r", free: 70 }] });
      if (short.canUninstall) {
        throw new Error("offering an uninstall that cannot help is a dead end");
      }
      const fresh = recover({ id: "demo", need: 100, installed: 0, roots: [{ root: "/r", free: 70 }] });
      if (fresh.canUninstall) {
        throw new Error("a fresh install has nothing to uninstall");
      }
      const unknown = recover(null);
      if (unknown.canUninstall || unknown.free !== null) {
        throw new Error("no data means no offer");
      }
      return "the recovery offer follows the host's numbers";
    },
    mutate(page) {
      return page.replace("free + installed >= need", "free >= need");
    },
  },
  {
    id: "space-refusal-asks-and-keeps-config",
    run(page) {
      const upload = functionBody(page, "uploadInstall");
      if (!upload.includes("refusal.status === 507")) {
        throw new Error("a 507 must be recognised by status, not by its message");
      }
      if (!upload.includes("askToUninstall(refusal, file, root, backup)")) {
        throw new Error("a 507 must open the recovery dialog");
      }
      if (upload.includes("errorText(response)")) {
        throw new Error("the 507 body must be read as data, not flattened to a sentence");
      }
      const dialog = functionBody(page, "askToUninstall");
      if (!dialog.includes("'DELETE'")) {
        throw new Error("the recovery must uninstall the old version");
      }
      if (/purge=true/.test(dialog)) {
        throw new Error("the uninstall must keep config/ and data/: no purge");
      }
      return "a 507 opens a dialog whose uninstall keeps the plugin's config";
    },
    mutate(page) {
      return page.replace(
        "'/api/v1/plugins/' + encodeURIComponent(plan.id)",
        "'/api/v1/plugins/' + encodeURIComponent(plan.id) + '?purge=true'",
      );
    },
  },
  {
    id: "space-recovery-retries-the-same-install",
    run(page) {
      const upload = functionBody(page, "uploadInstall");
      // A backup import runs the same room check, so it gets the same offer;
      // `&& !backup` would hand it a toast while the dialog could have fixed it.
      if (/!\s*backup/.test(upload)) {
        throw new Error("a space refusal must offer recovery for a backup import too");
      }
      const dialog = functionBody(page, "askToUninstall");
      if (!dialog.includes("uploadInstall(file, backup, root)")) {
        throw new Error("the dialog must re-send the same file after the uninstall");
      }
      if (!dialog.includes("if (!response.ok) throw")) {
        throw new Error("a failed uninstall must stop before the retry");
      }
      return "a 507 retries the install that just failed, backups included";
    },
    mutate(page) {
      return page.replace("        uploadInstall(file, backup, root);\n", "");
    },
  },
  {
    id: "space-recovery-counts-the-checked-root",
    run(page) {
      const recover = new Function("data", functionBody(page, "spaceRecovery"));
      // The uninstall returns the old copy's bytes to the root the check ran
      // on; a roomier volume elsewhere must not turn a dead end into an offer.
      const elsewhere = recover({
        id: "demo",
        need: 100,
        installed: 20,
        root: "/full",
        roots: [{ root: "/full", free: 70 }, { root: "/roomy", free: 900 }],
      });
      if (elsewhere.free !== 70) {
        throw new Error("the checked root's free bytes are the ones an uninstall refills");
      }
      if (elsewhere.canUninstall) {
        throw new Error("another root's room cannot make the uninstall worth offering");
      }
      const same = recover({
        id: "demo",
        need: 100,
        installed: 40,
        root: "/full",
        roots: [{ root: "/full", free: 70 }, { root: "/roomy", free: 900 }],
      });
      if (!same.canUninstall) {
        throw new Error("the checked root's own numbers can still justify the offer");
      }
      return "the offer adds the uninstall to the root the check ran on";
    },
    mutate(page) {
      return page.replace("row.root !== checked", "false");
    },
  },
  {
    id: "list-fills-the-page",
    run(page) {
      const wrap = cssRule(page, ".wrap");
      if (!wrap || !/min-height:\s*100vh/.test(wrap) || !/display:\s*flex/.test(wrap)) {
        throw new Error("the page column must fill the viewport");
      }
      const card = cssRule(page, ".list-card");
      if (!card || !/flex:\s*1/.test(card)) {
        throw new Error("the plugin list card must take the leftover height");
      }
      if (!page.includes('class="app-foot"')) {
        throw new Error("the source link belongs in the card footer");
      }
      return "the list card fills the page down to a footer with the same gap";
    },
    mutate(page) {
      return page.replace("  .list-card { flex:1 1 auto;", "  .list-card {");
    },
  },
  {
    id: "plugin-icons-fall-back",
    run(page) {
      const body = functionBody(page, "render");
      if (!body.includes("if (plugin.icon)")) {
        throw new Error("a row must use the icon the host reports");
      }
      if (!body.includes("icon.src = plugin.icon")) {
        throw new Error("the row must point the image at the host icon URL");
      }
      if (!body.includes("icon.onerror") || !body.includes("icon.replaceWith(mark)")) {
        throw new Error("a missing icon must fall back to the letter tile");
      }
      return "plugin rows use the package icon and keep the letter fallback";
    },
    mutate(page) {
      return page.replace("      icon.onerror = function () { icon.replaceWith(mark); };\n", "");
    },
  },
  {
    id: "settings-booleans-are-switches",
    run(page) {
      if (!page.includes("function settingsSwitch")) {
        throw new Error("boolean rows need a switch, not a two-option form control");
      }
      const field = functionBody(page, "field");
      if (!field.includes("settingsSwitch(entry, mark, unmark)")) {
        throw new Error("the bool row must render the switch");
      }
      const body = functionBody(page, "settingsSwitch");
      for (const needle of ["input.type = 'checkbox'", "mark(entry.key", "unmark(entry.key"]) {
        if (!body.includes(needle)) throw new Error("the switch is not wired: " + needle);
      }
      return "boolean settings render as switches";
    },
    mutate(page) {
      return page.replace(
        "control.appendChild(settingsSwitch(entry, mark, unmark));",
        "control.appendChild(document.createElement('select'));",
      );
    },
  },
  {
    id: "state-column-not-runtime",
    run(page) {
      if (!page.includes('<th scope="col">状态</th>')) {
        throw new Error("the status column is still labelled 运行");
      }
      if (!page.includes("content:'状态'")) {
        throw new Error("the mobile row label must follow the table header");
      }
      return "the column is named 状态 on desktop and mobile";
    },
    mutate(page) {
      return page.replaceAll("状态", "运行");
    },
  },
  {
    id: "empty-state-hides-with-rows",
    run(page) {
      const rule = cssRule(page, ".empty.hidden");
      if (!rule || !/display:\s*none/.test(rule)) {
        throw new Error("the empty state must lose to the hidden class when rows exist");
      }
      return "the empty state is hidden as soon as the list has rows";
    },
    mutate(page) {
      return page.replace("  .empty.hidden { display:none; }\n", "");
    },
  },
  {
    id: "settings-head-is-bare",
    run(page) {
      for (const removed of ["svcd · 配置", "悬停参数旁的 ?", "sheet-kicker"]) {
        if (page.includes(removed)) {
          throw new Error("the settings head still carries " + removed);
        }
      }
      const wait = functionBody(page, "waitForRestart");
      if (!wait.includes("status.textContent")) {
        throw new Error("the restart status must move into the sheet footer");
      }
      return "the settings head has only its title";
    },
    mutate(page) {
      return page.replace(
        "  head.className = 'sheet-head';",
        "  head.className = 'sheet-head';\n  const kicker = 'svcd · 配置';",
      );
    },
  },
  {
    id: "trusted-keys-live-in-security",
    run(page) {
      if (page.includes('role="menuitem" onclick="pickTrust()"')) {
        throw new Error("adding a public key must not stay in the overflow menu");
      }
      if (!page.includes("function trustCard")) {
        throw new Error("the Security page must render the trusted-key card");
      }
      if (!page.includes("if (group === '安全') page.appendChild(trustCard());")) {
        throw new Error("the trusted-key card must be attached to the Security group");
      }
      if (!page.includes("/api/v1/trust") || !page.includes("function removeTrust")) {
        throw new Error("the list and its delete action must use the trust API");
      }
      const remove = functionBody(page, "removeTrust");
      for (const needle of ["method: 'DELETE'", "confirm(", "refresh();"]) {
        if (!remove.includes(needle)) {
          throw new Error("deleting a key is not wired: " + needle);
        }
      }
      return "trusted keys are listed and removable from Security";
    },
    mutate(page) {
      return page.replace(
        "if (group === '安全') page.appendChild(trustCard());",
        "",
      );
    },
  },
  {
    id: "about-tab-does-not-save",
    run(page) {
      const body = functionBody(page, "showTab");
      if (!body.includes("save.hidden = group === '关于'")) {
        throw new Error("the About page must not offer a save button");
      }
      return "the About page has no save action";
    },
    mutate(page) {
      return page.replace(
        "    if (save) save.hidden = group === '关于' && Object.keys(changes).length === 0;\n",
        "",
      );
    },
  },
  {
    id: "about-page-carries-the-source",
    run(page) {
      if (!page.includes("const aboutGroup = '关于';")) {
        throw new Error("the settings sheet needs an About page");
      }
      for (const url of [
        "https://github.com/webxxxxx/plugin-manager",
        "https://github.com/webxxxxx/plugins",
      ]) {
        if (!page.includes(url)) throw new Error("the About page is missing " + url);
      }
      if (!page.includes("about-link")) {
        throw new Error("the About links must use the shared link style");
      }
      return "the About page carries the project and plugin source addresses";
    },
    mutate(page) {
      return page.replaceAll("https://github.com/webxxxxx/plugin-manager", "https://example.com/project");
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
