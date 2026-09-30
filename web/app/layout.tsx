import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: {
    default: "nanovm — sub-second JVM cold-start for enterprise Java",
    template: "%s · nanovm",
  },
  description:
    "KVM microVM snapshot/restore + MAP_PRIVATE fork-many for enterprise Java workloads. ~200 ms Spring Boot cold-start, no code changes, any JDK.",
  metadataBase: new URL("https://nanovm.io"),
  openGraph: {
    title: "nanovm — sub-second JVM cold-start for enterprise Java",
    description:
      "KVM microVM snapshot/restore + MAP_PRIVATE fork-many for enterprise Java workloads.",
    url: "https://nanovm.io",
    siteName: "nanovm",
    type: "website",
  },
  twitter: {
    card: "summary_large_image",
    title: "nanovm — sub-second JVM cold-start for enterprise Java",
    description:
      "KVM microVM snapshot/restore + MAP_PRIVATE fork-many for enterprise Java workloads.",
  },
  robots: { index: true, follow: true },
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en">
      <body className="antialiased">{children}</body>
    </html>
  );
}
