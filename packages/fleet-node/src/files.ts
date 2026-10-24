import { open } from "node:fs/promises";

export async function syncDirectory(path: string) {
  // Windows does not expose POSIX directory fsync; Linux Store volumes do.
  if (process.platform === "win32") return;
  const directory = await open(path, "r");
  try {
    await directory.sync();
  } finally {
    await directory.close();
  }
}
