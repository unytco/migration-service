export const jsonResp = (status: number, body: unknown) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });

export interface ReleaseFixture {
  tag: string;
  draft?: boolean;
  prerelease?: boolean;
  assets?: Array<{ name: string; url: string; digest?: string }>;
}

/** A GitHub releases listing of `repo`, newest first. */
export const releasesResp = (
  rels: ReleaseFixture[],
  repo = "unytco/unyt-sandbox",
) =>
  jsonResp(
    200,
    rels.map((r) => ({
      tag_name: r.tag,
      draft: r.draft ?? false,
      prerelease: r.prerelease ?? false,
      html_url: `https://github.com/${repo}/releases/tag/${r.tag}`,
      assets: (r.assets ?? []).map((a) => ({
        name: a.name,
        browser_download_url: a.url,
        ...(a.digest ? { digest: a.digest } : {}),
      })),
    })),
  );
