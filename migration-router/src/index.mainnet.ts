import registryJson from "../registry.mainnet.json";
import type { RawRegistry } from "./registry";
import { workerFor } from "./worker";

export default workerFor(registryJson as RawRegistry);
