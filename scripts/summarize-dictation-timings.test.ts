import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { generateSynthetic } from "./generate-synthetic-dictation-timings";
import {
  durationBucket,
  nearestRank,
  regressionThreshold,
  run,
  stats,
} from "./summarize-dictation-timings";

const FIXTURE = join(
  import.meta.dir,
  "fixtures",
  "dictation-timings",
  "synthetic-baseline.jsonl",
);

function withTempDir<T>(fn: (dir: string) => T): T {
  const dir = mkdtempSync(join(tmpdir(), "dictation-timings-test-"));
  try {
    return fn(dir);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

function summarizeText(
  text: string,
  extra: string[] = [],
  samples = "30",
): { code: number; output: string } {
  return withTempDir((dir) => {
    const file = join(dir, "in.jsonl");
    writeFileSync(file, text);
    const lines: string[] = [];
    const code = run(
      [
        "--input",
        file,
        "--samples",
        samples,
        "--warmup",
        "3",
        "--allow-synthetic",
        "--languages",
        "en",
        ...extra,
      ],
      (l) => lines.push(l),
    );
    return { code, output: lines.join("\n") };
  });
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type Patch = (r: any) => void;

function mutateLine(text: string, index: number, patch: Patch): string {
  const lines = text.trimEnd().split("\n");
  const record = JSON.parse(lines[index]);
  patch(record);
  lines[index] = JSON.stringify(record);
  return lines.join("\n") + "\n";
}

const baselineText = generateSynthetic();
// Each of the 5 cold starts is a record plus its session-end marker.
const FIRST_WARM = 10;

describe("statistics", () => {
  test("nearest-rank percentiles", () => {
    const ten = Array.from({ length: 10 }, (_, i) => i + 1);
    expect(nearestRank(ten, 50)).toBe(5);
    expect(nearestRank(ten, 95)).toBe(10);
    const twenty = Array.from({ length: 20 }, (_, i) => i + 1);
    expect(nearestRank(twenty, 95)).toBe(19);
    expect(nearestRank([7], 95)).toBe(7);
    expect(stats([30, 10, 20])).toEqual({
      n: 3,
      median: 20,
      p95: 30,
      min: 10,
      max: 30,
    });
  });

  test("duration buckets", () => {
    expect(durationBucket(5100, [5, 15, 60])).toBe(5);
    expect(durationBucket(14000, [5, 15, 60])).toBe(15);
    expect(durationBucket(61000, [5, 15, 60])).toBe(60);
    expect(durationBucket(30000, [5, 15, 60])).toBeNull();
  });

  test("regression threshold is max(10%, 50 ms)", () => {
    expect(regressionThreshold(100_000)).toBe(50_000);
    expect(regressionThreshold(1_000_000)).toBe(100_000);
  });
});

describe("summarizer CLI", () => {
  test("committed fixture is exactly the generator output", () => {
    expect(readFileSync(FIXTURE, "utf8")).toBe(baselineText);
  });

  test("fixture passes with 30 warm samples per workload and 5 cold", () => {
    const { code, output } = summarizeText(baselineText, ["--cold", "5"]);
    expect(output).toContain("SYNTHETIC DATA");
    expect(output).toContain("en raw 5s: n=30");
    expect(output).toContain("en cleanup 60s: n=30");
    expect(output).toContain("cold starts: n=5");
    expect(output).toContain("invariant violations: 0");
    expect(code).toBe(0);
  });

  test("asking for more samples than exist fails", () => {
    const { code, output } = summarizeText(baselineText, [], "31");
    expect(output).toContain("need 31");
    expect(code).toBe(1);
  });

  test("a missing language workload fails", () => {
    const { code, output } = summarizeText(baselineText, [
      "--languages",
      "en,pl",
    ]);
    expect(output).toContain("pl raw 5s: MISSING");
    expect(code).toBe(1);
  });

  test("too few cold starts fails", () => {
    const { code, output } = summarizeText(baselineText, ["--cold", "6"]);
    expect(output).toContain("5 cold success(es), need 6");
    expect(code).toBe(1);
  });

  test("invalid JSON and invalid fields fail", () => {
    expect(summarizeText(baselineText + "{not json\n").code).toBe(1);
    const bad = mutateLine(baselineText, FIRST_WARM, (r) => {
      r.total_us = -1;
    });
    const result = summarizeText(bad);
    expect(result.output).toContain("total_us must be a non-negative integer");
    expect(result.code).toBe(1);
  });

  test("a missing record is detected from the sequence gap", () => {
    const lines = baselineText.trimEnd().split("\n");
    lines.splice(FIRST_WARM + 10, 1);
    const result = summarizeText(lines.join("\n") + "\n");
    expect(result.output).toContain("are missing");
    expect(result.code).toBe(1);
  });

  test("dropped records reported by the app fail", () => {
    const text = mutateLine(baselineText, FIRST_WARM + 1, (r) => {
      r.dropped_records = 2;
    });
    const result = summarizeText(text);
    expect(result.output).toContain("were dropped");
    expect(result.code).toBe(1);
  });

  const cases: [string, Patch][] = [
    ["llm_request_in_raw_mode", (r) => (r.llm_requests = 1)],
    ["duplicate_paste", (r) => (r.paste_calls = 2)],
    ["multiple_active_operations", (r) => (r.active_ops = 2)],
    ["reload_during_warm_utterance", (r) => (r.worker_spawns_delta = 1)],
    ["engine_pending_exceeded", (r) => (r.engine_pending_at_stt = 3)],
    ["stage_sum_exceeds_total", (r) => (r.total_us = 1000)],
  ];
  test.each(cases)("invariant %s fails the run", (name, patch) => {
    const text = mutateLine(baselineText, FIRST_WARM + 2, patch);
    const result = summarizeText(text);
    expect(result.output).toContain(`${name}=1`);
    expect(result.code).toBe(1);
  });

  test("failures are counted per workload without failing the run", () => {
    // Append a cancelled 5 s raw operation after the last warm record.
    const lines = baselineText.trimEnd().split("\n");
    const endIndex = lines.length - 1;
    const last = JSON.parse(lines[endIndex - 1]);
    const end = JSON.parse(lines[endIndex]);
    const failed = {
      ...last,
      seq: last.seq + 1,
      op_id: last.op_id + 1,
      mode: "raw",
      outcome: "cancelled",
      llm_requests: 0,
      audio_ms: 5000,
    };
    end.records += 1;
    lines.splice(endIndex, 1, JSON.stringify(failed), JSON.stringify(end));
    const result = summarizeText(lines.join("\n") + "\n");
    expect(result.output).toContain("failures: cancelled=1");
    expect(result.code).toBe(0);
  });

  test("synthetic data fails unless explicitly allowed", () => {
    const strict = withTempDir((dir) => {
      const file = join(dir, "in.jsonl");
      writeFileSync(file, baselineText);
      const lines: string[] = [];
      const code = run(
        ["--input", file, "--samples", "30", "--languages", "en"],
        (l) => lines.push(l),
      );
      return { code, output: lines.join("\n") };
    });
    expect(strict.output).toContain("synthetic records are not measurements");
    expect(strict.code).toBe(1);
  });

  test("a lost first record of a session is detected", () => {
    const lines = baselineText.trimEnd().split("\n");
    lines.splice(FIRST_WARM, 1);
    const result = summarizeText(lines.join("\n") + "\n");
    expect(result.output).toContain("records 1..1 are missing");
    expect(result.code).toBe(1);
  });

  test("a lost last record of a session is detected from the end marker", () => {
    const lines = baselineText.trimEnd().split("\n");
    lines.splice(lines.length - 2, 1);
    const result = summarizeText(lines.join("\n") + "\n");
    expect(result.output).toContain("end marker says");
    expect(result.code).toBe(1);
  });

  test("a session without an end marker is reported", () => {
    const lines = baselineText.trimEnd().split("\n");
    lines.pop();
    const result = summarizeText(lines.join("\n") + "\n");
    expect(result.output).toContain("no end marker");
    expect(result.code).toBe(1);
  });

  test("language auto without a label cannot pass", () => {
    const text = mutateLine(baselineText, FIRST_WARM + 3, (r) => {
      r.language = "auto";
      delete r.label;
    });
    const result = summarizeText(text);
    expect(result.output).toContain('language "auto" and no label');
    expect(result.code).toBe(1);
  });

  test("two spoken languages are separate workloads that both must be present", () => {
    const both = generateSynthetic({ languages: ["en", "pl"] });
    const ok = summarizeText(both, ["--languages", "en,pl"]);
    expect(ok.output).toContain("pl raw 5s: n=30");
    expect(ok.code).toBe(0);
    // Dropping the pl session leaves en samples only: required pl is missing.
    const lines = both.trimEnd().split("\n");
    const onlyEn = lines.filter(
      (l) =>
        !l.includes('"label":"pl"') && !l.includes('"session":1700000001001'),
    );
    const missing = summarizeText(onlyEn.join("\n") + "\n", [
      "--languages",
      "en,pl",
    ]);
    expect(missing.output).toContain("pl raw 5s: MISSING");
    expect(missing.code).toBe(1);
    // An unexpected label is flagged instead of silently merged.
    const unexpected = summarizeText(both, ["--languages", "en"]);
    expect(unexpected.output).toContain(
      'unexpected workload language/label "pl"',
    );
    expect(unexpected.code).toBe(1);
  });

  test("protocol defaults require en and pl, 3 warmups and 5 cold starts", () => {
    const run30 = (text: string, extra: string[] = []) =>
      withTempDir((dir) => {
        const file = join(dir, "in.jsonl");
        writeFileSync(file, text);
        const lines: string[] = [];
        const code = run(
          ["--input", file, "--samples", "30", "--allow-synthetic", ...extra],
          (l) => lines.push(l),
        );
        return { code, output: lines.join("\n") };
      });
    // The spec's own command on an English-only file must not pass.
    const englishOnly = run30(baselineText);
    expect(englishOnly.output).toContain("pl raw 5s: MISSING");
    expect(englishOnly.code).toBe(1);
    // Both languages, 3 warmups and 5 cold starts present: passes by default.
    const full = run30(generateSynthetic({ languages: ["en", "pl"] }));
    expect(full.output).toContain("warmup dropped: 3");
    expect(full.output).toContain("cold starts: n=5");
    expect(full.code).toBe(0);
    // Default warmup 3: a file with only 30 samples per workload comes up short.
    const noWarmupsText = generateSynthetic({
      languages: ["en", "pl"],
      warmup: 0,
    });
    const noWarmups = run30(noWarmupsText);
    expect(noWarmups.output).toContain("need 30");
    expect(noWarmups.code).toBe(1);
    // Default cold 5: fewer cold starts fail, and --cold overrides it.
    const fewCold = generateSynthetic({ languages: ["en", "pl"], cold: 2 });
    expect(run30(fewCold).output).toContain("2 cold success(es), need 5");
    expect(run30(fewCold).code).toBe(1);
    expect(run30(fewCold, ["--cold", "2"]).code).toBe(0);
    expect(run30(noWarmupsText, ["--warmup", "0"]).code).toBe(0);
  });

  test("usage errors exit 2", () => {
    const lines: string[] = [];
    const log = (l: string) => lines.push(l);
    expect(run([], log)).toBe(2);
    expect(run(["--input", "x"], log)).toBe(2);
    expect(run(["--input", "x", "--samples", "0"], log)).toBe(2);
    expect(run(["--input", "x", "--samples", "1", "--bogus", "1"], log)).toBe(
      2,
    );
  });

  test("an unreadable input fails", () => {
    const lines: string[] = [];
    const missing = join(tmpdir(), "does-not-exist.jsonl");
    expect(
      run(["--input", missing, "--samples", "30"], (l) => lines.push(l)),
    ).toBe(1);
  });
});

describe("baseline comparison", () => {
  function compareWith(candidateText: string): {
    code: number;
    output: string;
  } {
    return withTempDir((dir) => {
      const base = join(dir, "base.jsonl");
      const cand = join(dir, "cand.jsonl");
      writeFileSync(base, baselineText);
      writeFileSync(cand, candidateText);
      const lines: string[] = [];
      const code = run(
        [
          "--input",
          cand,
          "--baseline",
          base,
          "--samples",
          "30",
          "--warmup",
          "3",
          "--allow-synthetic",
          "--languages",
          "en",
        ],
        (l) => lines.push(l),
      );
      return { code, output: lines.join("\n") };
    });
  }

  test("a required workload missing from the baseline fails the comparison", () => {
    const both = generateSynthetic({ languages: ["en", "pl"] });
    const result = withTempDir((dir) => {
      const base = join(dir, "base.jsonl");
      const cand = join(dir, "cand.jsonl");
      writeFileSync(base, baselineText); // English only
      writeFileSync(cand, both);
      const lines: string[] = [];
      const code = run(
        [
          "--input",
          cand,
          "--baseline",
          base,
          "--samples",
          "30",
          "--allow-synthetic",
        ],
        (l) => lines.push(l),
      );
      return { code, output: lines.join("\n") };
    });
    expect(result.output).toContain("pl raw 5s: no samples in baseline");
    expect(result.code).toBe(1);
  });

  test("identical data has no regression", () => {
    const { code, output } = compareWith(baselineText);
    expect(output).toContain("regression vs baseline");
    expect(output).not.toContain("REGRESSION");
    expect(code).toBe(0);
  });

  test("a 50% slower candidate regresses", () => {
    const { code, output } = compareWith(
      generateSynthetic({ scale: 1.5, cold: 5 }),
    );
    expect(output).toContain("REGRESSION");
    expect(code).toBe(1);
  });

  test("a 2% slower candidate stays inside the allowance", () => {
    const { code, output } = compareWith(
      generateSynthetic({ scale: 1.02, cold: 5 }),
    );
    expect(output).not.toContain("REGRESSION");
    expect(code).toBe(0);
  });
});
