// Registry: the version-upgrade chain + per-DNA notary daemon endpoints.
// Bundled with the Worker (imported JSON) or loaded from KV. The router
// validates it once at load; an invalid registry is a hard startup error.

export interface NotaryEntry {
  /** Cloudflare-Tunnel URL of a notary daemon serving this DNA, e.g. https://notary-1-v01.unyt.dev */
  url: string;
  /** Daemon HTTP API version the router speaks to this daemon (e.g. "v1"). */
  api: string;
}

/** Daemon HTTP API versions this router build knows how to speak. A registry
 * pinning anything else fails at startup, never at request time. */
export const SUPPORTED_DAEMON_APIS: ReadonlySet<string> = new Set(["v1"]);

export interface DnaEntry {
  dna_hash: string;
  /** Human-readable label, surfaced as from_version in /v1/migration-options. */
  version: string;
  /** dna_hash of the immediate predecessor; absent on chain roots. */
  upgrades_from?: string;
  /** Where to download the build for this DNA (e.g. a GitHub release page). Surfaced by /v1/update-check. */
  release_url?: string;
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
      // The router must know where to reach each daemon and which API to speak
      // from the registry alone — a deficient entry fails here at startup.
      for (const n of d.notaries) {
        if (!n.url?.startsWith("https://")) {
          throw new Error(`registry entry ${d.dna_hash}: notary url must be https`);
        }
        if (!SUPPORTED_DAEMON_APIS.has(n.api)) {
          throw new Error(`registry entry ${d.dna_hash}: unsupported notary api ${n.api}`);
        }
      }
    }
    // upgrades_from resolves, and each predecessor has at most one successor
    // (the chain is linear — forward lookup must be unambiguous).
    const successorOfHash = new Map<string, string>();
    for (const d of raw.dnas) {
      if (d.upgrades_from) {
        if (!seen.has(d.upgrades_from)) {
          throw new Error(`upgrades_from ${d.upgrades_from} (on ${d.dna_hash}) does not resolve`);
        }
        const existing = successorOfHash.get(d.upgrades_from);
        if (existing) {
          throw new Error(
            `${d.upgrades_from} has multiple successors (${existing} and ${d.dna_hash})`,
          );
        }
        successorOfHash.set(d.upgrades_from, d.dna_hash);
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

  /** The immediate successor of `fromDnaHash` — the entry that upgrades_from it, if any.
   * `load()` guarantees at most one, so this forward lookup is unambiguous. */
  successorOf(fromDnaHash: string): DnaEntry | undefined {
    for (const entry of this.byHash.values()) {
      if (entry.upgrades_from === fromDnaHash) return entry;
    }
    return undefined;
  }
}
