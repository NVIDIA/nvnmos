// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

(function () {
	'use strict';

	var GITHUB_REPO = 'https://github.com/NVIDIA/nvnmos';
	var OCTOCAT_SVG =
		'<svg class="nvnmos-github-icon" viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">' +
		'<path fill="currentColor" d="M8 0c4.42 0 8 3.58 8 8a8.013 8.013 0 0 1-5.45 7.59c-.4.08-.55-.17-.55-.38' +
		' 0-.27.01-1.13.01-2.2 0-.75-.25-1.29-.54-1.55 1.78-.2 3.65-.88 3.65-3.95 0-.88-.31-1.59-.82-2.15.08-.2.36-1.02-.08-2.12' +
		' 0 0-.67-.22-2.2.82-.64-.18-1.32-.27-2-.27-.68 0-1.36.09-2 .27-1.53-1.03-2.2-.82-2.2-.82-.44 1.1-.16 1.92-.08 2.12-.51.56-.82 1.28-.82 2.15' +
		' 0 3.06 1.86 3.75 3.64 3.95-.23.2-.44.55-.51 1.07-.46.21-1.61.55-2.33-.66-.15-.24-.6-.83-1.23-.82-.67-.01-.27.41 0 .59.34.19.73.9.82 1.13.16.39.68 1.31 2.69.94' +
		' 0 .67.01 1.3.01 1.49 0 .21-.15.45-.55.38A7.995 7.995 0 0 1 0 8c0-4.42 3.58-8 8-8Z"></path></svg>';

	// The theme's styleswitcher eagerly persists 'hotdoc.style' on load, so a
	// missing key cannot be used to detect a first visit. Track our own marker
	// instead: apply light once, then leave the user's later choice alone.
	function defaultToLightMode() {
		if (!window.localStorage || typeof setActiveStyleSheet !== 'function') {
			return;
		}
		if (localStorage.getItem('nvnmos.default-style') === 'applied') {
			return;
		}
		localStorage.setItem('nvnmos.default-style', 'applied');
		setActiveStyleSheet('light');
	}

	function addGitHubNavLink() {
		if (document.getElementById('extra-menu')) {
			return;
		}

		var wrapper = document.getElementById('navbar-wrapper');
		if (!wrapper) {
			return;
		}

		var menu = document.createElement('ul');
		menu.className = 'nav navbar-nav navbar-right';
		menu.id = 'extra-menu';
		menu.innerHTML =
			'<li><a class="nvnmos-github-link" href="' + GITHUB_REPO +
			'" title="NvNmos on GitHub">' + OCTOCAT_SVG +
			'<span>View on GitHub</span></a></li>';

		var center = wrapper.querySelector('.navbar-center');
		if (center) {
			wrapper.insertBefore(menu, center);
		} else {
			wrapper.appendChild(menu);
		}
	}

	defaultToLightMode();

	if (window.jQuery) {
		jQuery(document).ready(addGitHubNavLink);
	} else {
		document.addEventListener('DOMContentLoaded', addGitHubNavLink);
	}
})();
