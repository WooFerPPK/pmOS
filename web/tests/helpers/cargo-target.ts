import path from "node:path";

export function resolveCargoTargetDirectory(
  workspaceRoot: string,
  configuredTargetDirectory: string | undefined,
): string {
  if (configuredTargetDirectory === undefined) {
    return path.join(workspaceRoot, "target");
  }
  if (configuredTargetDirectory.length === 0) {
    throw new Error("CARGO_TARGET_DIR is set to an empty string");
  }
  if (path.isAbsolute(configuredTargetDirectory)) {
    return configuredTargetDirectory;
  }
  return path.join(workspaceRoot, configuredTargetDirectory);
}
