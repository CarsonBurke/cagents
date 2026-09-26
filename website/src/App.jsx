import { useState } from "react";
import installCommand from "./install-command.sh?raw";

const features = [
  {
    number: "01",
    title: "Sessions that stay organized",
    description:
      "Keep repositories, agents, and Git worktrees visible in one workspace instead of scattered across terminal windows.",
  },
  {
    number: "02",
    title: "Terminals that keep running",
    description:
      "A persistent per-user daemon keeps terminal sessions alive while the interface restarts or gets out of the way.",
  },
  {
    number: "03",
    title: "The tools beside the work",
    description:
      "Open embedded browser surfaces and GitHub pull requests without losing the context of the session that created them.",
  },
];

function GithubIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <path
        fill="currentColor"
        d="M12 .7a11.5 11.5 0 0 0-3.64 22.4c.58.1.79-.25.79-.56v-2.23c-3.22.7-3.9-1.37-3.9-1.37-.52-1.34-1.28-1.7-1.28-1.7-1.05-.72.08-.71.08-.71 1.16.08 1.77 1.19 1.77 1.19 1.03 1.77 2.7 1.26 3.36.96.1-.75.4-1.26.73-1.55-2.57-.29-5.27-1.29-5.27-5.69 0-1.26.45-2.29 1.19-3.1-.12-.29-.52-1.47.11-3.06 0 0 .97-.31 3.16 1.18a10.9 10.9 0 0 1 5.76 0c2.19-1.49 3.16-1.18 3.16-1.18.63 1.59.23 2.77.11 3.06.74.81 1.19 1.84 1.19 3.1 0 4.42-2.71 5.39-5.29 5.68.42.36.79 1.07.79 2.16v3.2c0 .31.21.67.8.56A11.5 11.5 0 0 0 12 .7Z"
      />
    </svg>
  );
}

function ArrowIcon() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true">
      <path d="m4 12 8-8m-6 0h6v6" />
    </svg>
  );
}

function App() {
  const [copied, setCopied] = useState(false);

  async function copyInstallCommand() {
    try {
      await navigator.clipboard.writeText(installCommand);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1800);
    } catch {
      setCopied(false);
    }
  }

  return (
    <div className="site-shell">
      <header className="nav">
        <a className="brand" href="#top" aria-label="cagents home">
          <img src={`${import.meta.env.BASE_URL}icon.svg`} alt="" />
          <span>cagents</span>
        </a>
        <nav aria-label="Primary navigation">
          <a href="#features">Features</a>
          <a href="#install">Install</a>
          <a
            className="github-link"
            href="https://github.com/CarsonBurke/cagents"
            target="_blank"
            rel="noreferrer"
          >
            <GithubIcon />
            GitHub
          </a>
        </nav>
      </header>

      <main id="top">
        <section className="hero" aria-labelledby="hero-heading">
          <div className="hero-copy">
            <p className="eyebrow">
              <span /> Native Linux workspace
            </p>
            <h1 id="hero-heading">
              Keep every coding agent <em>in context.</em>
            </h1>
            <p className="lede">
              cagents brings agent sessions, repositories, Git worktrees,
              persistent terminals, and browsers into one fast Wayland app.
            </p>

            <div className="hero-actions">
              <a className="primary-button" href="#install">
                Install cagents
                <span aria-hidden="true">↓</span>
              </a>
              <a
                className="text-link"
                href="https://github.com/CarsonBurke/cagents"
                target="_blank"
                rel="noreferrer"
              >
                View source <ArrowIcon />
              </a>
            </div>

            <p className="requirements">Linux x86_64 · glibc · Wayland</p>
          </div>

          <div
            className="product-frame"
            role="img"
            aria-label="A stylized cagents workspace"
          >
            <div className="window-bar">
              <span className="window-title">cagents</span>
              <span className="window-controls" aria-hidden="true">
                <i />
                <i />
                <i />
              </span>
            </div>
            <div className="workspace-preview">
              <aside className="preview-sidebar">
                <p className="preview-label">WORKSPACES</p>
                <div className="workspace-row active">
                  <span className="status-dot" />
                  cagents
                </div>
                <div className="workspace-row nested">
                  <span>⌞</span> landing-page
                </div>
                <p className="preview-label sessions-label">SESSIONS</p>
                <div className="session-row">
                  <span className="agent-mark">C</span>
                  <span>
                    <b>website</b>
                    <small>working</small>
                  </span>
                </div>
                <div className="session-row muted">
                  <span className="agent-mark">C</span>
                  <span>
                    <b>release-check</b>
                    <small>ready</small>
                  </span>
                </div>
              </aside>
              <div className="terminal-panel">
                <div className="tab-strip">
                  <span className="terminal-tab active">website</span>
                  <span className="terminal-tab">preview</span>
                  <span className="tab-add">+</span>
                </div>
                <div className="terminal-content" aria-hidden="true">
                  <p><span className="prompt">~/cagents</span> <span className="branch">main</span></p>
                  <p><span className="caret">❯</span> npm run dev</p>
                  <p className="terminal-muted">VITE ready in 184 ms</p>
                  <p className="terminal-url">http://localhost:5173/cagents/</p>
                  <p className="terminal-spacer" />
                  <p><span className="caret">❯</span> <span className="cursor" /></p>
                </div>
                <div className="status-bar">
                  <span>terminal 1</span>
                  <span>main</span>
                </div>
              </div>
            </div>
          </div>
        </section>

        <section className="features" id="features" aria-labelledby="features-heading">
          <div className="section-heading">
            <p className="eyebrow"><span /> One workspace, less overhead</p>
            <h2 id="features-heading">Built around the way agent work actually branches.</h2>
          </div>
          <div className="feature-grid">
            {features.map((feature) => (
              <article key={feature.number}>
                <span className="feature-number">{feature.number}</span>
                <h3>{feature.title}</h3>
                <p>{feature.description}</p>
              </article>
            ))}
          </div>
        </section>

        <section className="install-section" id="install" aria-labelledby="install-heading">
          <div className="install-copy">
            <p className="eyebrow"><span /> Get cagents</p>
            <h2 id="install-heading">Install with your GitHub account.</h2>
            <p>
              Sign in with <code>gh auth login</code> using an account with access
              to the private repository. The command installs the latest release
              for your user. No root required.
            </p>
          </div>

          <div className="command-card">
            <div className="command-header">
              <span>Terminal</span>
              <button type="button" onClick={copyInstallCommand}>
                {copied ? "Copied" : "Copy"}
              </button>
            </div>
            <pre><code><span>$</span> {installCommand}</code></pre>
            <p>
              Pins one release and verifies the installer and archive against
              its SHA-256 manifest before running anything. See the{" "}
              <a href="https://github.com/CarsonBurke/cagents/blob/main/docs/installation.md#install-with-the-verified-installer">
                installation guide
              </a>{" "}
              for requirements and source inspection.
            </p>
          </div>
        </section>
      </main>

      <footer>
        <a className="brand footer-brand" href="#top">
          <img src={`${import.meta.env.BASE_URL}icon.svg`} alt="" />
          <span>cagents</span>
        </a>
        <p>Native workspace orchestration for coding agents.</p>
        <a
          className="footer-source"
          href="https://github.com/CarsonBurke/cagents"
          target="_blank"
          rel="noreferrer"
        >
          Source <ArrowIcon />
        </a>
      </footer>
    </div>
  );
}

export default App;
