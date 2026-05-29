// Registry: the version-upgrade chain + per-DNA notary daemon endpoints.
// Bundled with the Worker (imported JSON) or loaded from KV. The router
// validates it once at load; an invalid registry is a hard startup error.

export interface NotaryEntry {
  /** Cloudflare-Tunnel URL of a notary daemon serving this DNA, e.g. https://notary-1-v01.unyt.dev */
  url: string;
}

export interface DnaEntry {
  dna_hash: string;
  /** Human-readable label, surfaced as from_version in /v1/migration-options. */
  version: string;
  /** dna_hash of the immediate predecessor; absent on chain roots. */
  upgrades_from?: string;
  /** 1..N notary daemons serving this DNA (redundancy / failover). */
  notaries: NotaryEntry[];
}

export interface RawRegistry {
  /** Registry schema version; the router refuses unknown values. */
  version: number;
  dnas: DnaEntry[];
}

export const SUPPORTED_REGISTRY_VERSION = 1;

export class Registry {
  private byHash: Map<string, DnaEntry>;

  private constructor(
    public readonly version: number,
    dnas: DnaEntry[],
  ) {
    this.byHash = new Map(dnas.map((d) => [d.dna_hash, d]));
  }

  /** Parse + validate. Throws on any invariant violation (caller fails the Worker health). */
  static load(raw: RawRegistry): Registry {
    if (raw.version !== SUPPORTED_REGISTRY_VERSION) {
      throw new Error(
        `unsupported registry version ${raw.version} (expected ${SUPPORTED_REGISTRY_VERSION})`,
      );
    }
    if (!Array.isArray(raw.dnas)) throw new Error("registry.dnas must be an array");

    const seen = new Set<string>();
    for (const d of raw.dnas) {
      if (!d.dna_hash) throw new Error("registry entry missing dna_hash");
      if (seen.has(d.dna_hash)) throw new Error(`duplicate dna_hash ${d.dna_hash}`);
      seen.add(d.dna_hash);
      if (!d.version) throw new Error(`registry entry ${d.dna_hash} missing version`);
      if (!Array.isArray(d.notaries)) throw new Error(`registry entry ${d.dna_hash} missing notaries`);
    }
    // upgrades_from resolves
    for (const d of raw.dnas) {
      if (d.upgrades_from && !seen.has(d.upgrades_from)) {
        throw new Error(`upgrades_from ${d.upgrades_from} (on ${d.dna_hash}) does not resolve`);
      }
    }
    // no cycles — walk each chain back to a root
    const byHash = new Map(raw.dnas.map((d) => [d.dna_hash, d]));
    for (const start of raw.dnas) {
      const path = new Set<string>();
      let cur: DnaEntry | undefined = start;
      while (cur) {
        if (path.has(cur.dna_hash)) {
          throw new Error(`cycle in upgrades_from chain at ${cur.dna_hash}`);
        }
        path.add(cur.dna_hash);
        cur = cur.upgrades_from ? byHash.get(cur.upgrades_from) : undefined;
      }
    }
    return new Registry(raw.version, raw.dnas);
  }

  get(dnaHash: string): DnaEntry | undefined {
    return this.byHash.get(dnaHash);
  }

  /** The immediate predecessor of `toDnaHash`, if registered. */
  predecessorOf(toDnaHash: string): DnaEntry | undefined {
    const entry = this.byHash.get(toDnaHash);
    if (!entry?.upgrades_from) return undefined;
    return this.byHash.get(entry.upgrades_from);
  }
}
