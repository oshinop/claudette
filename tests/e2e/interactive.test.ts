import { expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { launch, onRemote, plain, project, projectKey, scenario, until } from "./harness";

test("interactive claudette: Claude Code's own interface from the remote machine, tools on the local machine", async () => {
  const dir = project();
  const id = await scenario([
    { tool: "Write", input: { file_path: join(dir, "tui.txt"), content: "from the TUI" } },
    { tool: "Bash", input: { command: "hostname" } },
    { text: "INTERACTIVE-DONE on {{last_tool_result}}" },
  ]);

  let screen = "";
  const decoder = new TextDecoder();
  const log = join(mkdtempSync(join(tmpdir(), "log-")), "claudette.log");
  const proc = Bun.spawn(["claudette", "--permission-mode", "acceptEdits", "--allowedTools", "Bash"], {
    cwd: dir,
    env: { ...process.env, TERM: "xterm-256color", CLAUDETTE_LOG: log },
    terminal: { cols: 140, rows: 45, data: (_t, d) => void (screen += decoder.decode(d, { stream: true })) },
  });
  const toolsListed = () =>
    existsSync(log) && readFileSync(log, "utf8").split("\n").some((l) => l.includes('"dir":"mcp-in:cc"') && l.includes("notifications/initialized"));

  const end = Date.now() + 60_000;
  let trusted = false;
  let typed = false;
  let lastKey = 0;
  while (Date.now() < end && !plain(screen).includes("INTERACTIVE-DONE")) {
    const text = plain(screen);
    if (!trusted && text.includes("Entertoconfirm") && Date.now() - lastKey > 500) {
      const highlighted = text.slice(text.lastIndexOf("❯") + 1);
      lastKey = Date.now();
      if (highlighted.startsWith("Yes")) {
        trusted = true;
        proc.terminal!.write("\r");
      } else {
        proc.terminal!.write("\x1b[B");
      }
    }
    if (trusted && !typed && text.includes("accepteditson") && toolsListed()) {
      typed = true;
      proc.terminal!.write(`[[scenario:${id}]] go`);
      await Bun.sleep(300);
      proc.terminal!.write("\r");
    }
    await Bun.sleep(100);
  }
  const text = plain(screen);
  expect(text.includes("INTERACTIVE-DONEonlocal"), text.slice(-3000)).toBe(true);
  expect(readFileSync(join(dir, "tui.txt"), "utf8")).toBe("from the TUI");

  proc.terminal!.write("\x03");
  await Bun.sleep(300);
  proc.terminal!.write("\x03");
  const exited = await Promise.race([proc.exited, Bun.sleep(15_000).then(() => "timeout")]);
  if (exited === "timeout") proc.kill();
  expect(exited, plain(screen).slice(-2000)).not.toBe("timeout");
  expect(await until(() => onRemote("ls /tmp/claudette-* 2>/dev/null | wc -l") === "0", 5_000)).toBe(true);

  const folder = join(homedir(), ".claude", "projects", projectKey(dir));
  const [mine] = readdirSync(folder).filter((f) => f.endsWith(".jsonl"));
  expect(mine).toBeDefined();
  expect(onRemote("find ~/.claude/projects -name '*.jsonl' | wc -l")).toBe("0");
  const earlier = crypto.randomUUID();
  const copied = readFileSync(join(folder, mine!), "utf8").replaceAll(mine!.replace(".jsonl", ""), earlier).replaceAll(`[[scenario:${id}]] go`, "BEFORE-THE-SWITCH");
  writeFileSync(join(folder, `${earlier}.jsonl`), copied);

  screen = "";
  const picker = Bun.spawn(["claudette", "--resume"], {
    cwd: dir,
    env: { ...process.env, TERM: "xterm-256color" },
    terminal: { cols: 140, rows: 45, data: (_t, d) => void (screen += decoder.decode(d, { stream: true })) },
  });
  const listed = () => plain(screen).includes("BEFORE-THE-SWITCH") && plain(screen).includes(`[[scenario:${id}]]go`);
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline && !listed()) await Bun.sleep(100);
  expect(listed(), plain(screen).slice(-2000)).toBe(true);
  picker.terminal!.write("\x1b");
  await Bun.sleep(300);
  picker.terminal!.write("\x03");
  if ((await Promise.race([picker.exited, Bun.sleep(10_000).then(() => "timeout")])) === "timeout") picker.kill();
}, 120_000);

test("what a terminal session saves to its user settings lands in this machine's, under the built-in names; the remote machine's own settings stay untouched", async () => {
  const remoteSettings = "$HOME/.claude/settings.json";
  const remoteBefore = onRemote(`cat "${remoteSettings}" 2>/dev/null || echo ABSENT`);
  const localSettings = join(homedir(), ".claude", "settings.json");
  const localBefore = existsSync(localSettings) ? readFileSync(localSettings, "utf8") : undefined;
  mkdirSync(join(homedir(), ".claude"), { recursive: true });
  writeFileSync(localSettings, JSON.stringify({ env: { KEPT_HERE: "1" } }));
  try {
    const session = await launch(project(), [], "shift+tabtocycle");
    expect(session.text(), session.text().slice(-2000)).toContain("shift+tabtocycle");
    const config = onRemote("ls -d /tmp/claudette-*.config | head -n 1");
    await session.type("/theme");
    expect(await session.waitFor("Lightmode", 10_000), session.text().slice(-2000)).toBe(true);
    session.write("\x1b[B");
    expect(await session.waitFor("❯Lightmode", 10_000), session.text().slice(-2000)).toBe(true);
    session.write("\r");
    expect(await until(() => onRemote(`cat ${config}/settings.json`).includes('"light"'), 10_000)).toBe(true);
    const given = JSON.parse(onRemote(`cat ${config}/settings.json`));
    given.permissions = { ...given.permissions, allow: [...(given.permissions?.allow ?? []), "mcp__cc__Bash(make:*)"] };
    onRemote(`printf '%s' '${JSON.stringify(given)}' > ${config}/settings.json.tmp.1.abcdef && mv ${config}/settings.json.tmp.1.abcdef ${config}/settings.json`);
    await session.exit();

    const saved = JSON.parse(readFileSync(localSettings, "utf8"));
    expect(saved.permissions.allow).toContain("Bash(make:*)");
    expect(saved.theme).toBe("light");
    expect(saved.env).toEqual({ KEPT_HERE: "1" });
    expect(onRemote(`cat "${remoteSettings}" 2>/dev/null || echo ABSENT`)).toBe(remoteBefore);
  } finally {
    if (localBefore === undefined) rmSync(localSettings, { force: true });
    else writeFileSync(localSettings, localBefore);
  }
}, 90_000);

test("← and /bg never hand a session to the remote machine's own background service", async () => {
  const session = await launch(project(), [], "shift+tabtocycle");
  session.write("\x1b[D");
  await Bun.sleep(2000);
  await session.type("/bg");
  await Bun.sleep(3000);
  expect(session.text()).toContain("Unknowncommand:/bg");
  await session.exit();
  expect(onRemote("pgrep -f 'claude[.]exe daemon' | wc -l")).toBe("0");
  expect(onRemote("ls ~/.claude/jobs 2>/dev/null | wc -l")).toBe("0");
}, 90_000);
