// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLlmsTxt from 'starlight-llms-txt';

export default defineConfig({
  site: 'https://claude-code-proxy.raine.dev',
  integrations: [
    starlight({
      title: 'claude-code-proxy',
      description: 'Run Claude Code with Codex, Kimi, Grok, or Cursor Agent.',
      plugins: [starlightLlmsTxt()],
      favicon: '/favicon.svg',
      head: [
        {
          tag: 'script',
          attrs: {
            src: '/image-zoom.js',
            defer: true,
          },
        },
      ],
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/raine/claude-code-proxy',
        },
      ],
      components: {
        SocialIcons: './src/components/HeaderLinks.astro',
      },
      customCss: ['./src/styles/site.css'],
      sidebar: [
        {
          label: 'Start here',
          items: [
            { label: 'What is claude-code-proxy?', link: '/' },
            { label: 'Getting started', slug: 'getting-started' },
            { label: 'How it works', slug: 'how-it-works' },
          ],
        },
        {
          label: 'Providers',
          items: [
            { label: 'Choosing a provider', slug: 'providers/choosing-a-provider' },
            { label: 'Codex', slug: 'providers/codex' },
            { label: 'Kimi', slug: 'providers/kimi' },
            { label: 'Grok', slug: 'providers/grok' },
            { label: 'Cursor Agent', slug: 'providers/cursor-agent' },
          ],
        },
        {
          label: 'Using the proxy',
          items: [
            { label: 'Configure Claude Code', slug: 'using/configure-claude-code' },
            { label: 'Models and routing', slug: 'using/models-and-routing' },
            { label: 'Switching models and backends', slug: 'using/switching-models-and-backends' },
            { label: 'Monitor TUI', slug: 'using/monitor-tui' },
            { label: 'For coding agents', slug: 'using/for-coding-agents' },
            { label: 'Troubleshooting', slug: 'using/troubleshooting' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'Command reference', slug: 'reference/command-reference' },
            { label: 'Configuration', slug: 'reference/configuration' },
            { label: 'Files and storage', slug: 'reference/files-and-storage' },
            { label: 'HTTP API', slug: 'reference/http-api' },
            { label: 'Compatibility and limitations', slug: 'reference/compatibility-and-limitations' },
            { label: 'Changelog', slug: 'reference/changelog' },
          ],
        },
      ],
    }),
  ],
});
