import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "Engine Box · Fleet",
  description: "Your machines, workspaces, and native coding agents.",
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
