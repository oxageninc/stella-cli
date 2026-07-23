import { ImageResponse } from "next/og";

/**
 * Neutral, Vercel-style social card, generated at build time (next/og) so it
 * stays in sync with the brand and carries no static binary. Pure monochrome:
 * snow on black, one hairline border — the same paper-&-ink system as the site.
 */
export const alt =
  "Stella — a fast, BYOK, model-agnostic terminal coding agent that proves its work";
export const size = { width: 1200, height: 630 };
export const contentType = "image/png";

export default function OpengraphImage() {
  return new ImageResponse(
    (
      <div
        style={{
          width: "100%",
          height: "100%",
          display: "flex",
          flexDirection: "column",
          justifyContent: "space-between",
          background: "#000000",
          color: "#ededed",
          padding: "80px",
        }}
      >
        <div style={{ display: "flex", alignItems: "center" }}>
          <div
            style={{
              display: "flex",
              alignItems: "center",
              gap: "14px",
              border: "1px solid #333333",
              borderRadius: "9999px",
              padding: "12px 26px",
              fontSize: "28px",
              color: "#a1a1a1",
            }}
          >
            <span style={{ color: "#ededed", fontWeight: 700 }}>{">_"}</span>
            <span>a terminal coding agent</span>
          </div>
        </div>

        <div style={{ display: "flex", flexDirection: "column" }}>
          <div
            style={{
              display: "flex",
              fontSize: "168px",
              fontWeight: 800,
              letterSpacing: "-6px",
              lineHeight: 1,
            }}
          >
            stella
          </div>
          <div
            style={{
              display: "flex",
              marginTop: "30px",
              fontSize: "46px",
              color: "#a1a1a1",
              maxWidth: "940px",
              lineHeight: 1.25,
            }}
          >
            Ship code from your terminal — and let a verifier decide when the
            work is actually done.
          </div>
        </div>

        <div
          style={{
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
            fontSize: "28px",
            color: "#808080",
          }}
        >
          <div style={{ display: "flex" }}>stella.oxagen.sh</div>
          <div style={{ display: "flex" }}>
            BYOK · model-agnostic · local-first
          </div>
        </div>
      </div>
    ),
    { ...size },
  );
}
