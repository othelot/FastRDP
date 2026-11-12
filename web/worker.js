// Cloudflare Worker for fastrdp.dev
//
//   PowerShell (irm https://fastrdp.dev | iex)  -> install.ps1 of the latest release, as text
//   https://fastrdp.dev/install.ps1             -> same, for any client
//   https://fastrdp.dev/v1.2.3                  -> install.ps1 of that release (used for elevation)
//   browsers                                    -> the GitHub repository
const REPO = "othelot/fastrdp";
const SCRIPT = { "content-type": "text/plain; charset=utf-8", "x-content-type-options": "nosniff" };

export default {
  async fetch(request) {
    const url = new URL(request.url);
    const powershell = /PowerShell/i.test(request.headers.get("user-agent") || "");
    const pinned = url.pathname.match(/^\/v(\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?)$/);

    let source;
    if (pinned) {
      source = `https://github.com/${REPO}/releases/download/v${pinned[1]}/install.ps1`;
    } else if (url.pathname === "/install.ps1" || (url.pathname === "/" && powershell)) {
      source = `https://github.com/${REPO}/releases/latest/download/install.ps1`;
    } else if (url.pathname === "/") {
      return Response.redirect(`https://github.com/${REPO}`, 302);
    } else {
      return new Response("Not found\n", { status: 404 });
    }

    const r = await fetch(source, { cf: { cacheTtl: 300, cacheEverything: true } });
    if (!r.ok) {
      // still valid PowerShell, so `| iex` prints a clear message
      return new Response(`throw 'FastRDP: install script not available (${r.status}). See https://github.com/${REPO}/releases'\n`, { status: 502, headers: SCRIPT });
    }
    return new Response(await r.text(), { headers: { ...SCRIPT, "cache-control": "public, max-age=300" } });
  },
};
