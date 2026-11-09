// Picks the page's language before it paints. A choice made in the
// language menu is remembered and always wins; with none, a browser whose
// preferred language is Japanese opens the Japanese page once. The pages
// are /, and /ja/; nothing here runs without the menu's own links working.
(function () {
  var KEY = "gpui-maps-lang";
  var page = document.documentElement.lang === "ja" ? "ja" : "en";
  var root = page === "ja" ? "../" : "./";
  var target = { en: root, ja: root + "ja/" };
  var chosen = null;
  try { chosen = localStorage.getItem(KEY); } catch (e) {}
  var want = chosen;
  if (!want) {
    var langs = navigator.languages && navigator.languages.length ? navigator.languages : [navigator.language || ""];
    for (var i = 0; i < langs.length; i++) {
      var l = String(langs[i]).toLowerCase();
      if (l.indexOf("ja") === 0) { want = "ja"; break; }
      if (l.indexOf("en") === 0) { want = "en"; break; }
    }
    // Auto-detection only ever moves an English visitor to Japanese;
    // someone who opened /ja/ directly stays there.
    if (want !== "ja" || page === "ja") want = null;
  }
  if (want && want !== page && target[want]) {
    location.replace(target[want] + location.search + location.hash);
  }
})();
