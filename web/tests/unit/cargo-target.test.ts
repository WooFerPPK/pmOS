import path from "node:path";
import { describe, expect, it } from "vitest";
import { resolveCargoTargetDirectory } from "../helpers/cargo-target";

describe("resolveCargoTargetDirectory", () => {
  const root = path.parse(process.cwd()).root;
  const workspaceRoot = path.join(root, "workspace");

  it("uses the workspace target directory when the variable is unset", () => {
    expect(resolveCargoTargetDirectory(workspaceRoot, undefined)).toBe(
      path.join(workspaceRoot, "target"),
    );
  });

  it("resolves a relative target directory from the workspace Cargo cwd", () => {
    const configuredTargetDirectory = path.join("artifacts", "cargo");
    const resolved = resolveCargoTargetDirectory(
      workspaceRoot,
      configuredTargetDirectory,
    );

    expect(resolved).toBe(path.join(workspaceRoot, configuredTargetDirectory));
    expect(resolved).not.toBe(
      path.join(process.cwd(), configuredTargetDirectory),
    );
  });

  it("leaves an absolute target directory unchanged", () => {
    const configuredTargetDirectory = path.join(
      root,
      "var",
      "tmp",
      "pmos-target",
    );

    expect(
      resolveCargoTargetDirectory(workspaceRoot, configuredTargetDirectory),
    ).toBe(configuredTargetDirectory);
  });

  it("rejects an explicitly empty target directory", () => {
    expect(() => resolveCargoTargetDirectory(workspaceRoot, "")).toThrow(
      "CARGO_TARGET_DIR is set to an empty string",
    );
  });
});
