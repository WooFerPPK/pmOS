import { expect, type Page } from "@playwright/test";

export interface Region {
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
}

export interface CausalLatencySample {
  readonly id: number;
  readonly kind: "key" | "drag" | "focus";
  readonly totalMs: number;
  readonly inputToMainMs: number;
  readonly mainPaintMs: number;
  readonly sequence: number;
  readonly presentations: number;
}

type CausalEvidence =
  | {
      readonly kind: "fingerprint";
      readonly region: Region;
      readonly before: number;
    }
  | {
      readonly kind: "pixel";
      readonly point: { readonly x: number; readonly y: number };
      readonly expected: readonly number[];
    }
  | {
      readonly kind: "pixels";
      readonly samples: readonly {
        readonly point: { readonly x: number; readonly y: number };
        readonly expected: readonly number[];
      }[];
    };

export interface ArmSampleOptions {
  readonly id: number;
  readonly kind: CausalLatencySample["kind"];
  readonly input: "keydown" | "pointerdown" | "pointermove";
  readonly code?: string;
  readonly notBefore: number;
  readonly evidence: CausalEvidence;
}

export async function regionFingerprint(
  page: Page,
  region: Region,
): Promise<number> {
  return page
    .locator("#pmos-fb")
    .evaluate((canvas: HTMLCanvasElement, sample: Region) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      const bytes = context.getImageData(
        sample.x,
        sample.y,
        sample.width,
        sample.height,
      ).data;
      let hash = 0x811c9dc5;
      for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193);
      return hash >>> 0;
    }, region);
}

export async function armCausalSample(
  page: Page,
  options: ArmSampleOptions,
): Promise<void> {
  await page
    .locator("#pmos-fb")
    .evaluate(async (canvas: HTMLCanvasElement, sample) => {
      if (performance.now() < sample.notBefore) {
        await new Promise<void>((resolve) => {
          const advance = (now: number): void => {
            if (now >= sample.notBefore) {
              resolve();
            } else {
              requestAnimationFrame(advance);
            }
          };
          requestAnimationFrame(advance);
        });
      }

      const resultKey = "pmosLatencySample";
      delete canvas.dataset[resultKey];
      let startedAt: number | null = null;
      let sequenceAtInput: number | null = null;

      const fingerprint = (region: Region): number => {
        const context = canvas.getContext("2d");
        if (context === null) throw new Error("framebuffer 2d context missing");
        const bytes = context.getImageData(
          region.x,
          region.y,
          region.width,
          region.height,
        ).data;
        let hash = 0x811c9dc5;
        for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193);
        return hash >>> 0;
      };

      const evidencePresented = (): boolean => {
        if (sample.evidence.kind === "fingerprint") {
          return fingerprint(sample.evidence.region) !== sample.evidence.before;
        }
        const context = canvas.getContext("2d");
        if (context === null) throw new Error("framebuffer 2d context missing");
        const probes =
          sample.evidence.kind === "pixel"
            ? [sample.evidence]
            : sample.evidence.samples;
        return probes.every((probe) => {
          const current = Array.from(
            context.getImageData(probe.point.x, probe.point.y, 1, 1).data,
          );
          return current.every(
            (channel, index) => channel === probe.expected[index],
          );
        });
      };

      const onInput = (event: Event): void => {
        if (
          sample.input === "keydown" &&
          (!(event instanceof KeyboardEvent) || event.code !== sample.code)
        ) {
          return;
        }
        if (
          sample.input === "pointerdown" &&
          (!(event instanceof PointerEvent) || event.button !== 0)
        ) {
          return;
        }
        if (
          sample.input === "pointermove" &&
          (!(event instanceof PointerEvent) || event.buttons !== 1)
        ) {
          return;
        }
        startedAt = performance.now();
        sequenceAtInput = Number(canvas.dataset.pmosFrameSequence ?? "0");
        window.removeEventListener(sample.input, onInput, true);
      };

      const onFrame = (event: Event): void => {
        const detail = (
          event as CustomEvent<{
            sequence: number;
            receivedAt: number;
            paintedAt: number;
          }>
        ).detail;
        if (
          startedAt === null ||
          sequenceAtInput === null ||
          detail.sequence <= sequenceAtInput ||
          !evidencePresented()
        ) {
          return;
        }
        canvas.removeEventListener("pmos:frame", onFrame);
        window.removeEventListener(sample.input, onInput, true);
        canvas.dataset[resultKey] = JSON.stringify({
          id: sample.id,
          kind: sample.kind,
          totalMs: detail.paintedAt - startedAt,
          inputToMainMs: detail.receivedAt - startedAt,
          mainPaintMs: detail.paintedAt - detail.receivedAt,
          sequence: detail.sequence,
          presentations: detail.sequence - sequenceAtInput,
        } satisfies CausalLatencySample);
      };

      window.addEventListener(sample.input, onInput, true);
      canvas.addEventListener("pmos:frame", onFrame);
    }, options);
}

export async function readCausalSample(
  page: Page,
  id: number,
  consoleLines: readonly string[],
): Promise<CausalLatencySample> {
  const canvas = page.locator("#pmos-fb");
  try {
    const handle = await page.waitForFunction(
      (expectedId) => {
        const framebuffer =
          document.querySelector<HTMLCanvasElement>("#pmos-fb");
        const encoded = framebuffer?.dataset.pmosLatencySample;
        if (encoded === undefined) return null;
        const sample = JSON.parse(encoded) as CausalLatencySample;
        return sample.id === expectedId ? sample : null;
      },
      id,
      { timeout: 2_000, polling: "raf" },
    );
    const sample = (await handle.jsonValue()) as CausalLatencySample;
    await handle.dispose();
    expect(
      sample.totalMs,
      `sample ${id} input-to-pixel latency`,
    ).toBeGreaterThanOrEqual(0);
    expect(
      sample.inputToMainMs,
      `sample ${id} input-to-main latency`,
    ).toBeGreaterThanOrEqual(0);
    expect(
      sample.mainPaintMs,
      `sample ${id} main-thread paint latency`,
    ).toBeGreaterThanOrEqual(0);
    expect(
      sample.presentations,
      `sample ${id} completed presentations`,
    ).toBeGreaterThanOrEqual(1);
    return sample;
  } catch (cause) {
    const sequence = await canvas.getAttribute("data-pmos-frame-sequence");
    throw new Error(
      `causal latency sample ${id} did not complete; frame_sequence=${sequence ?? "missing"}\n` +
        consoleLines.slice(-50).join("\n"),
      { cause },
    );
  }
}

export function percentile(
  samples: readonly number[],
  proportion: number,
): number {
  if (samples.length === 0)
    throw new Error("cannot compute an empty percentile");
  const sorted = [...samples].sort((left, right) => left - right);
  return sorted[Math.ceil(sorted.length * proportion) - 1]!;
}
