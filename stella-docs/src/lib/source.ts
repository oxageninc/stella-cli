import { docs } from "@/.source/server";
import { loader } from "fumadocs-core/source";
import { createElement } from "react";
import { ProviderIcon } from "@/components/provider-logos";

export const source = loader({
  baseUrl: "/docs",
  source: docs.toFumadocsSource(),
  // Frontmatter `icon: <provider-id>` puts the vendor's logomark next to the
  // page in the sidebar — unknown ids fall through to no icon.
  icon(icon) {
    if (icon) return createElement(ProviderIcon, { id: icon });
  },
});
