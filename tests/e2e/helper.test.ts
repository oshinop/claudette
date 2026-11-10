import { describe, expect, test } from "bun:test";
import { appendFileSync, existsSync } from "node:fs";
import { homedir } from "node:os";
import { basename, join } from "node:path";
import { onRemote, project, projectKey, resultText, scenario, start, until } from "./harness";

const MOCK = process.env.MOCK_API ?? "http://mockapi:8080";

/** The ssh connections carrying this container's sessions to their holders. */
const cutTheLink = () => Bun.spawnSync(["pkill", "-f", "__helper connect"]);
const holdersOnRemote = () => onRemote("pgrep -f '__helper [h]old' | wc -l");
const runFilesOnRemote = () => onRemote("ls /tmp | grep -c '^claudette-' || true");

describe("the session helper", () => {
  test("the binary is copied once and cached on the remote machine, and nothing of a run stays behind", async () => {
    const dir = project();
    for (const n of [1, 2]) {
      const run = Bun.spawnSync(["claudette", "-p", `run ${n}`], { cwd: dir, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
      expect(run.exitCode, run.stderr.toString()).toBe(0);
    }
    expect(onRemote("ls ~/.claudette/bin | wc -l")).toBe("1");
    expect(onRemote("ls ~/.claudette/bin/*/claudette | wc -l")).toBe("1");
    // The holder goes once the goodbye reaches it, a moment after claudette has exited.
    expect(await until(() => holdersOnRemote() === "0" && runFilesOnRemote() === "0", 5_000)).toBe(true);
  });

  test("a run that copies the binary first still carries a resumed transcript larger than ssh takes in before the helper answers", async () => {
    const dir = project();
    const id = await scenario([{ text: "turn-done" }]);
    const run = (args: string[]) => Bun.spawnSync(["claudette", "-p", ...args, "--output-format", "json"], { cwd: dir, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
    const first = run([`[[scenario:${id}]] the codeword is medlar`]);
    expect(first.exitCode, first.stderr.toString()).toBe(0);
    const session: unknown = JSON.parse(first.stdout.toString()).session_id;
    expect(typeof session).toBe("string");
    // About 3 MiB, sent whole with the run's first frames: more than ssh reads before the remote
    // machine says the binary is missing and hangs up.
    const summary = `${JSON.stringify({ type: "summary", summary: "x".repeat(1000), leafUuid: "none" })}\n`;
    appendFileSync(join(homedir(), ".claude", "projects", projectKey(dir), `${session}.jsonl`), summary.repeat(3000));
    onRemote("rm -rf ~/.claudette/bin");
    const resumed = run(["--resume", String(session), `[[scenario:${id}]] what was it?`]);
    expect(resumed.exitCode, resumed.stderr.toString()).toBe(0);
    expect(JSON.parse(resumed.stdout.toString()).result).toBe("turn-done");
    expect(onRemote("ls ~/.claudette/bin/*/claudette | wc -l")).toBe("1");
  });

  test("a dropped connection does not end a host session: the tool call in flight finishes and the reply arrives", async () => {
    const dir = project();
    const id = await scenario([{ tool: "Bash", input: { command: "sleep 4; echo after-the-drop" } }, { text: "reply: {{last_tool_result}}" }]);
    const session = start(dir, { allowedTools: ["Bash"] });
    const turn = session.turn(`[[scenario:${id}]] wait it out`);
    await Bun.sleep(1500);
    cutTheLink();
    const result = await turn;
    await session.close();
    expect(resultText(result)).toBe("reply: after-the-drop");
    expect(session.stderr.join("")).toContain("was lost; the session waits");
    expect(session.stderr.join("")).toContain("reconnected to remote");
  });

  test("a -p run survives this machine going quiet: stopped, its link cut, then back", async () => {
    const dir = project();
    // The first Bash call marks that it started here, then runs long enough to be stopped mid-call.
    const id = await scenario([{ tool: "Bash", input: { command: "touch started && sleep 2 && echo before" } }, { tool: "Bash", input: { command: "echo after" } }, { text: "{{last_tool_result}}" }]);
    // While the first Bash call runs here, the process stops (as a local machine does in sleep) and the link drops.
    const proc = Bun.spawn(["claudette", "-p", `[[scenario:${id}]] go`, "--output-format", "json", "--allowedTools", "Bash"], { cwd: dir, stdout: "pipe", stderr: "pipe" });
    expect(await until(() => existsSync(join(dir, "started")))).toBe(true);
    process.kill(proc.pid, "SIGSTOP");
    cutTheLink();
    // A real outage: the stopped process has to stay away while the remote side notices the cut link.
    await Bun.sleep(3000);
    process.kill(proc.pid, "SIGCONT");
    expect(await proc.exited).toBe(0);
    expect((JSON.parse(await new Response(proc.stdout).text()) as { result: string }).result).toBe("after");
  });

  test("a session whose local machine never returns ends on the remote machine after the detach timeout, and cleans up", async () => {
    const dir = project();
    const id = await scenario([{ tool: "Bash", input: { command: "sleep 60" } }, { text: "done" }]);
    const proc = Bun.spawn(["claudette", "-p", `[[scenario:${id}]] go`, "--allowedTools", "Bash"], { cwd: dir, env: { ...process.env, CLAUDETTE_DETACH_TIMEOUT: "3s" }, stdout: "ignore", stderr: "ignore" });
    expect(await until(() => holdersOnRemote() === "1", 20_000)).toBe(true);
    // This machine's side disappears at once, as if the local machine lost power.
    proc.kill("SIGKILL");
    cutTheLink();
    expect(await until(() => holdersOnRemote() === "0" && runFilesOnRemote() === "0", 20_000)).toBe(true);
  });

  test("a message reaches a session that skips permission prompts, as from its own child", async () => {
    const target = project();
    const readyId = await scenario([{ text: "ready" }]);
    const a = start(target, { permissionMode: "bypassPermissions", allowDangerouslySkipPermissions: true } as never);
    await a.turn(`[[scenario:${readyId}]] stand by`);
    try {
      const marker = `BYPASS-${Math.random().toString(36).slice(2)}`;
      const id = await scenario([{ tool: "SendMessage", input: { to: basename(target), message: marker } }, { text: "{{last_tool_result}}" }]);
      const run = Bun.spawnSync(["claudette", "-p", `[[scenario:${id}]] tell`, "--output-format", "json"], { cwd: project(), stdout: "pipe", stderr: "pipe" });
      expect(run.exitCode, run.stderr.toString()).toBe(0);
      // Held, a message would never reach the model: a bypassing session shows no approval here.
      let delivered = false;
      for (let i = 0; i < 100 && !delivered; i++) {
        const all = (await (await fetch(`${MOCK}/_log`)).json()) as { messages: unknown[] }[];
        delivered = all.some((r) => JSON.stringify(r.messages).includes(marker) && JSON.stringify(r.messages).includes("Another Claude session"));
        if (!delivered) await Bun.sleep(200);
      }
      expect(delivered).toBe(true);
    } finally {
      await a.close();
    }
  });
});
