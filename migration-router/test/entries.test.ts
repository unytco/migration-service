import { describe, expect, it, vi } from "vitest";
import localExample from "../registry.local.example.json";
import type { RawRegistry } from "../src/registry";
import { workerFor } from "../src/worker";

const HTTP_NOTARY_REGISTRY = vi.hoisted(() => ({
  version: 1,
  dnas: [
    {
      dna_hash: "uhC0k_http_notary",
      version: "alliance-v1.0.0",
      notaries: [{ url: "http://10.0.0.1:8790", api: "v2" }],
    },
  ],
}));

vi.mock("../registry.json", () => ({ default: HTTP_NOTARY_REGISTRY }));
vi.mock("../registry.mainnet.json", () => ({ default: HTTP_NOTARY_REGISTRY }));

describe("deployed entry points", () => {
  it("TestNet refuses an http:// notary", async () => {
    await expect(import("../src/index")).rejects.toThrow(
      "notary url must be https",
    );
  });

  it("MainNet refuses an http:// notary", async () => {
    await expect(import("../src/index.mainnet")).rejects.toThrow(
      "notary url must be https",
    );
  });
});

describe("workerFor", () => {
  const local = localExample as RawRegistry;

  it("passes its load options on", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    try {
      expect(() => workerFor(local, { allowHttpNotaries: true })).not.toThrow();
    } finally {
      warn.mockRestore();
    }
  });

  it("warns when the registry still holds the placeholder DNA hash", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    try {
      workerFor(local, { allowHttpNotaries: true });
      expect(warn).toHaveBeenCalledWith(
        expect.stringContaining("UN-PROVISIONED"),
      );
    } finally {
      warn.mockRestore();
    }
  });
});
