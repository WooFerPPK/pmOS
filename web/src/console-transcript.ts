/** A minimal DOM text sink. `HTMLElement` and test fakes satisfy this shape. */
export interface ConsoleTranscriptSink {
  textContent: string | null;
}

/** Visible diagnostics retained in the browser DOM. */
export const CONSOLE_TRANSCRIPT_MAX_BYTES = 256 * 1024;
export const CONSOLE_TRANSCRIPT_MAX_LINES = 512;

export interface ConsoleTranscriptLimits {
  readonly maxBytes: number;
  readonly maxLines: number;
}

function tailByLines(text: string, maxLines: number): string {
  if (text === "" || maxLines <= 0) return "";
  let retainedLines = text.endsWith("\n") ? 0 : 1;
  let cursor = text.length;
  while (cursor > 0) {
    const newline = text.lastIndexOf("\n", cursor - 1);
    if (newline < 0) return text;
    retainedLines += 1;
    if (retainedLines > maxLines) return text.slice(newline + 1);
    cursor = newline;
  }
  return text;
}

function tailByUtf8Bytes(text: string, maxBytes: number): string {
  if (text === "" || maxBytes <= 0) return "";
  const encoded = new TextEncoder().encode(text);
  if (encoded.byteLength <= maxBytes) return text;

  let start = encoded.byteLength - maxBytes;
  // A UTF-8 suffix must begin at a code-point boundary. Dropping continuation
  // bytes retains a valid suffix and never grows beyond the byte ceiling.
  while (start < encoded.byteLength && (encoded[start]! & 0xc0) === 0x80) {
    start += 1;
  }
  return new TextDecoder().decode(encoded.subarray(start));
}

/** Keep the newest complete diagnostic tail within both hard ceilings. */
export function boundedConsoleTail(
  text: string,
  limits: ConsoleTranscriptLimits,
): string {
  return tailByUtf8Bytes(
    tailByLines(text, limits.maxLines),
    limits.maxBytes,
  );
}

/**
 * Bounded browser-console transcript renderer.
 *
 * The retained string never exceeds either ceiling. Rendering assigns the
 * bounded tail from private state instead of reading `sink.textContent` and
 * appending to it, avoiding the unbounded read/copy/write growth of
 * `textContent += chunk` during long-running sessions.
 */
export class ConsoleTranscript {
  private value = "";

  constructor(
    private readonly sink: ConsoleTranscriptSink,
    private readonly limits: ConsoleTranscriptLimits = {
      maxBytes: CONSOLE_TRANSCRIPT_MAX_BYTES,
      maxLines: CONSOLE_TRANSCRIPT_MAX_LINES,
    },
  ) {
    if (limits.maxBytes <= 0 || limits.maxLines <= 0) {
      throw new RangeError("console transcript limits must be positive");
    }
  }

  append(text: string): void {
    if (text === "") return;
    const incoming = boundedConsoleTail(text, this.limits);
    this.value = boundedConsoleTail(`${this.value}${incoming}`, this.limits);
    this.sink.textContent = this.value;
  }

  /** Current retained text, exposed for diagnostics and isolation tests. */
  get text(): string {
    return this.value;
  }
}
