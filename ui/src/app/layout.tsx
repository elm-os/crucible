import type { Metadata } from "next";
import type { ReactNode } from "react";
import { Providers } from "@/components/providers";
// oxlint-disable-next-line import/no-unassigned-import
import "./globals.css";

export const metadata: Metadata = {
  title: { default: "Crucible", template: "%s · Crucible" },
  description: "Self-hosted observability and analytics for AI agents and MCP servers.",
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en">
      <head>
        {/* Without a nonce, UI SDK adds its CSS as constructed style sheets, which a CSP that
            hashes inline scripts still allows. */}
        <meta property="csp-nonce" />
      </head>
      <body>
        <Providers>{children}</Providers>
      </body>
    </html>
  );
}
