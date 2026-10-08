// Last-resort watchdog for a stale PWA client. When a cached shell references a
// hashed asset the current build no longer has, the relay's SPA fallback answers
// the `/assets/index-OLD.js` request with `index.html` as `text/html` — a MIME
// mismatch, so the `<script type=module>` never runs and the in-app update banner
// (which needs the React bundle) can't render. This listener runs first, catches
// that load failure, nukes the SW + caches, and hard-reloads with a cache-bust.
;(() => {
	var recovered = false
	function recover(detail) {
		if (recovered) return
		recovered = true
		try {
			console.warn('[app] stale-client recovery:', detail)
		} catch {
			// console may be unavailable; ignore
		}
		var root = document.getElementById('root') || document.body
		while (root.firstChild) root.removeChild(root.firstChild)
		var wrap = document.createElement('div')
		wrap.style.cssText =
			'font-family:system-ui;min-height:100vh;display:flex;align-items:center;justify-content:center;padding:2rem;text-align:center;color:#eaeaf0;background:#0a0b0e'
		var inner = document.createElement('div')
		var title = document.createElement('div')
		title.style.cssText = 'font-size:1.25rem;margin-bottom:.5rem'
		title.textContent = 'Updating app…'
		var sub = document.createElement('div')
		sub.style.opacity = '.6'
		sub.textContent = 'Clearing cached version.'
		inner.appendChild(title)
		inner.appendChild(sub)
		wrap.appendChild(inner)
		root.appendChild(wrap)
		var done = function () {
			var sep = location.search ? '&' : '?'
			location.replace(location.pathname + location.search + sep + '_v=' + Date.now())
		}
		var tasks = []
		if ('serviceWorker' in navigator) {
			tasks.push(
				navigator.serviceWorker
					.getRegistrations()
					.then(function (regs) {
						return Promise.all(regs.map(function (r) {
							return r.unregister()
						}))
					})
					.catch(function () {
						// best-effort cleanup; reload anyway
					})
			)
		}
		if ('caches' in window) {
			tasks.push(
				caches
					.keys()
					.then(function (names) {
						return Promise.all(names.map(function (n) {
							return caches.delete(n)
						}))
					})
					.catch(function () {
						// best-effort cleanup; reload anyway
					})
			)
		}
		Promise.all(tasks).then(done, done)
	}
	window.addEventListener(
		'error',
		function (e) {
			var t = e.target
			if (!t || t === window) return
			var src = t.src || t.href
			if (!src || src.indexOf('/assets/') === -1) return
			if (navigator.onLine === false) return
			if (location.search.indexOf('_v=') !== -1) return
			recover('asset load failed: ' + src)
		},
		true
	)
})()
