import registryJson from "../registry.local.json";
import type { RawRegistry } from "./registry";
import { workerFor } from "./worker";

export default workerFor(registryJson as RawRegistry, {
  allowHttpNotaries: true,
});
