// clhorde docs — behaviors

(function () {
  'use strict';

  // Active nav highlighting
  var path = location.pathname;
  var links = document.querySelectorAll('.nav-links a:not(.github-link)');
  links.forEach(function (a) {
    var href = a.getAttribute('href');
    if (path.endsWith(href) || (href === 'index.html' && (path.endsWith('/') || path.endsWith('/docs/')))) {
      a.classList.add('active');
    }
  });

  // Mobile hamburger menu
  var hamburger = document.querySelector('.hamburger');
  var navLinks = document.querySelector('.nav-links');
  if (hamburger && navLinks) {
    hamburger.addEventListener('click', function () {
      navLinks.classList.toggle('open');
      hamburger.textContent = navLinks.classList.contains('open') ? '\u2715' : '\u2630';
    });
    // Close on link click
    navLinks.querySelectorAll('a').forEach(function (a) {
      a.addEventListener('click', function () {
        navLinks.classList.remove('open');
        hamburger.textContent = '\u2630';
      });
    });
  }

  // Copy buttons on <pre> blocks
  document.querySelectorAll('pre').forEach(function (pre) {
    var btn = document.createElement('button');
    btn.className = 'copy-btn';
    btn.textContent = 'Copy';
    btn.addEventListener('click', function () {
      var code = pre.querySelector('code');
      var text = (code || pre).textContent;
      navigator.clipboard.writeText(text).then(function () {
        btn.textContent = 'Copied!';
        btn.classList.add('copied');
        setTimeout(function () {
          btn.textContent = 'Copy';
          btn.classList.remove('copied');
        }, 1500);
      });
    });
    pre.appendChild(btn);
  });

  // Cheatsheet tab switching
  var tabBtns = document.querySelectorAll('.tab-btn');
  tabBtns.forEach(function (btn) {
    btn.addEventListener('click', function () {
      var target = btn.getAttribute('data-tab');
      tabBtns.forEach(function (b) { b.classList.remove('active'); });
      btn.classList.add('active');
      document.querySelectorAll('.tab-content').forEach(function (tc) {
        tc.classList.toggle('active', tc.id === target);
      });
    });
  });
})();
