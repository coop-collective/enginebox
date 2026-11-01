import type { Metadata, Viewport } from "next";
// Styrene typefaces, self-hosted (SIL OFL): the control plane makes no third-party font requests.
import "@fontsource-variable/schibsted-grotesk";
import "@fontsource-variable/jetbrains-mono";
import "./globals.css";

export const metadata: Metadata = {
  title: "Engine Box",
  description: "Orchestrate your machines, native coding agents and CI runners.",
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  themeColor: [
    { media: "(prefers-color-scheme: light)", color: "#e4e8eb" },
    { media: "(prefers-color-scheme: dark)", color: "#12171c" },
  ],
};

export default function RootLayout({
  children,
}: Readonly<{ children: React.ReactNode }>) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
