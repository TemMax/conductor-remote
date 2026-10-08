import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'
import { VitePWA } from 'vite-plugin-pwa'

// The relay serves the built `dist/` in production and does its own SPA fallback.
// In dev, Vite serves this directory with HMR and proxies /api to the relay so the
// phone can hit one origin. The `server` block is dev-only — `vite build` ignores
// it, so the prod bundle is unaffected.
const webPort = Number(process.env.WEB_PORT) || 5173
const relayPort = Number(process.env.RELAY_PORT) || 8790
const webModule = (path: string) => fileURLToPath(new URL(path, import.meta.url))

// Baked into the bundle as `__APP_VERSION__` so the running app knows which build it
// is — shown on the Connect sheet beside the relay version, and compared against the
// relay's reported version to force a service-worker update when this client is stale.
const { version: appVersion } = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')) as {
	version: string
}

export default defineConfig({
	define: {
		__APP_VERSION__: JSON.stringify(appVersion),
		__APP_RELEASES_URL__: JSON.stringify('https://github.com/TemMax/conductor-remote/releases')
	},
	resolve: {
		alias: [
			// Pierre intentionally exposes every Shiki grammar and theme as a dynamic
			// import. Workbox would consequently precache more than 11 MB even though
			// the phone only ever requests the grammar for the file on screen. Keep the
			// renderer intact while giving it the relay's deliberately bounded registry.
			{ find: /^shiki$/, replacement: webModule('./src/lib/syntax/pierre-shiki.ts') },
			{ find: /^shiki\/wasm$/, replacement: webModule('./src/lib/syntax/pierre-shiki-wasm.ts') },
			{ find: '@pierre/theming/themes', replacement: webModule('./src/lib/syntax/pierre-themes.ts') }
		]
	},
	// Repo-root `public/` (outside this directory) so Conductor's repo-icon lookup —
	// which only scans root-level paths like `public/apple-touch-icon.png` — finds
	// the same assets the PWA serves, with no duplicated icon file.
	publicDir: '../public',
	build: {
		outDir: 'dist',
		emptyOutDir: true,
		rollupOptions: {
			output: {
				// Grammar chunks are immutable, on-demand assets. Giving them a stable
				// directory lets Workbox leave them out of the install-time app shell.
				chunkFileNames: chunk =>
					chunk.moduleIds.some(
						id => id.includes('/node_modules/shiki/dist/langs/') || id.includes('/node_modules/@shikijs/langs/')
					)
						? 'assets/diff-syntax/[name]-[hash].js'
						: 'assets/[name]-[hash].js'
			}
		}
	},
	server: {
		host: true,
		port: webPort,
		strictPort: true,
		proxy: {
			'/api': { target: `http://127.0.0.1:${relayPort}`, changeOrigin: true }
		}
	},
	plugins: [
		react(),
		tailwindcss(),
		VitePWA({
			// `prompt`, not `autoUpdate`: autoUpdate forces skipWaiting + reloads the page
			// the instant a new SW activates, which can interrupt a prompt mid-compose. The
			// ReloadPrompt banner (src/components/ReloadPrompt.tsx) applies updates on tap.
			registerType: 'prompt',
			includeAssets: ['icon.svg', 'apple-touch-icon.png'],
			manifest: {
				name: 'Conductor Remote',
				short_name: 'Conductor',
				description: 'Monitor and drive your local Conductor agents from your phone.',
				start_url: '/',
				scope: '/',
				display: 'standalone',
				orientation: 'portrait',
				background_color: '#0a0b0e',
				theme_color: '#0a0b0e',
				icons: [
					{ src: '/icon-192.png', sizes: '192x192', type: 'image/png' },
					{ src: '/icon-512.png', sizes: '512x512', type: 'image/png' },
					{ src: '/icon-maskable-512.png', sizes: '512x512', type: 'image/png', purpose: 'maskable' },
					{ src: '/icon.svg', sizes: 'any', type: 'image/svg+xml' }
				]
			},
			workbox: {
				// Push + notification-click handlers. Workbox owns sw.js in generateSW mode, so this
				// is the seam for adding listeners to it (see public/push-sw.js). The file is also
				// precached, which is what makes an edit to it change sw.js and actually ship.
				importScripts: ['/push-sw.js'],
				navigateFallback: '/index.html',
				// Never cache the token-gated API — it must always hit the live relay.
				navigateFallbackDenylist: [/^\/api\//],
				// Claim open pages the moment a new worker activates. Still prompt-mode
				// (skipWaiting stays gated behind ReloadPrompt's SKIP_WAITING message), but
				// once the user taps Update the activated worker takes control of this page,
				// so `controllerchange` fires and the reload lands — iOS otherwise leaves the
				// new worker active-but-not-controlling and the tap looks like a no-op.
				clientsClaim: true,
				// Drop prior-build precache entries when a new SW activates, so a stale shell
				// can't linger and re-serve old hashed assets.
				cleanupOutdatedCaches: true,
				// A diff downloads only its own Shiki grammar. Cache that immutable chunk
				// after first use without making every phone fetch every language on update.
				globIgnores: ['**/assets/diff-syntax/**'],
				runtimeCaching: [
					{
						urlPattern: ({ url }) => url.pathname.startsWith('/assets/diff-syntax/'),
						handler: 'CacheFirst',
						options: {
							cacheName: 'diff-syntax',
							expiration: { maxEntries: 32, maxAgeSeconds: 60 * 60 * 24 * 365 }
						}
					}
				]
			}
		})
	]
})
