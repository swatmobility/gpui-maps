**DRAFT, pending legal review. Not yet in force.**

# SWAT trademark policy

The code of gpui-maps and the `swat-maps-*` crates is published under the
licence in this repository's licence files (MIT OR Apache-2.0). That licence
covers **copyright** in the code. It gives no rights in SWAT Mobility's
**trademarks**. This policy covers the trademarks.

The aim of this policy is simple: people who see the marks must be able to
trust that they point to SWAT and to the engine SWAT publishes. Most uses
need no permission. Section 4 lists them.

## 1. Definitions

1. **The marks** are:
   - the names **SWAT**, **SWAT Mobility** and **SWAT Maps**, in any
     typeface, and the abbreviation "SWAT" when it is used in a map,
     mobility or software context;
   - the SWAT logo: the symbol and the "SWAT" wordmark, alone or together,
     including the two artwork files the engine draws,
     `src/assets/swat-on-light@2x.png` and `src/assets/swat-on-dark@2x.png`;
   - any name, logo or design that is confusingly similar to these.
2. **SWAT** is SWAT Mobility Pte. Ltd., the owner of the marks.
3. **The engine** is the `gpui-maps` crate and the other code in this
   repository.
4. **You** are any person or organisation that uses the engine or the marks.

The marks are not covered by the code licence, by `NOTICE` or by
`ATTRIBUTION.md`. The crate names `gpui-maps` and `swat-maps-*` are names of
SWAT's packages; the parts of those names that are marks are covered by this
policy.

## 2. The logo on maps

The engine draws the SWAT logo on every map it renders, in the bottom-left
corner, by default. The logo says which engine drew the map. It does not say
that SWAT made, checked or endorses your application or the data on the map.

When the logo is shown on a map:

1. **Do not restyle it.** Do not recolour, redraw, stretch, crop, rotate,
   animate or outline it, and do not combine it with another mark. Use only
   the two artwork files that the engine ships.
2. **Keep it legible.** Do not cover it with other content, and do not make
   it so small or faint that it cannot be read.
3. **Adjust it only through the engine's public API.** You may change its
   size, opacity and artwork variant through `MapView::set_brand_mark` and
   `MapView::set_brand_mark_policy`. These changes are within this policy.
4. **Keep the data credits.** The tile source's attribution, which the
   engine draws bottom-right, belongs to the data providers, not to SWAT.
   This policy does not change what you owe them.
5. **Static, print and exported maps** that show the logo follow the same
   rules.

**You may turn the logo off.** Call `MapView::set_brand_mark(None)`. A map
without the logo needs no permission and no other credit to SWAT. We ask
that you credit the engine in your documentation or "about" page, but this
is a request, not a condition.

## 3. How to use the marks (all uses)

1. **Use the correct form.** Write the names in full, with their usual
   capitals: "SWAT", "SWAT Mobility", "SWAT Maps". Do not abbreviate them,
   join them to other words, or change their spelling.
2. **Add a notice outside SWAT's own materials.** Put it near the first use
   of a mark, or on an "about" page if space is short. A suggested notice:

   > SWAT and SWAT Maps are trademarks of SWAT Mobility Pte. Ltd. This
   > product uses the gpui-maps engine and is not made, endorsed or
   > sponsored by SWAT Mobility.

   One notice can cover all the marks. On a small screen, a ™ after the
   first use is sufficient if the full notice is on another page.
3. **Do not cause confusion.** No use may suggest that SWAT made, endorses,
   sponsors, certifies or is a partner of you or your product, unless that
   is true.

## 4. Uses that need no permission

1. **Running and shipping the engine.** Use, build and distribute the
   engine, unmodified or modified, inside your own application, with the
   logo shown as section 2 allows or turned off.
2. **Referring to the project.** Say truthfully that your software uses, is
   built on, is compatible with or is derived from gpui-maps or SWAT Maps,
   in text, documentation, talks and articles.
3. **News, reviews and research.** Use the marks in news, reviews,
   commentary, teaching and research about the project, including
   screenshots of maps that show the logo.
4. **Personal blogs and social media.** Make truthful statements about the
   project. Do not use the logo as your profile image, background or
   header, and do not name an account so that it looks official.
5. **Links.** Use the names, or the logo at its original proportions, to
   link to the project's repository or website.
6. **Software that works with the engine.** Name your own crate, plug-in,
   style or tool so that it says what it works with, for example
   "tilecache-for-gpui-maps". The name must not contain "SWAT", and the
   tool must not look like an official SWAT product.
7. **Forks.** Keep the logo files and this policy in a copy or fork of the
   repository, so that the repository stays complete. If you distribute a
   modified engine and keep the logo on, your distribution must not suggest
   that SWAT made or reviewed your changes. The simplest way for a fork is
   to turn the logo off, or to replace the two artwork files with your own
   mark.

## 5. Uses that need written permission

Ask first (see section 8). Tell us what the use is, who is behind it, and
where and for how long it will appear.

1. **Product, fork and package names.** Any product, fork, crate, package,
   application or service whose name contains a mark. A fork must not
   publish under `swat-maps-*` or any other name that contains "SWAT".
2. **Domain names and accounts.** A domain name, subdomain or social-media
   account that contains a mark.
3. **Events.** A public event or conference that has a mark in its name or
   uses the logo.
4. **Publications.** A book, course or video that has a mark in its title or
   the logo on its cover.
5. **Merchandise.** Goods, stickers or clothing with a mark that you sell.
   Items you make for yourself or give away at no profit need no
   permission.
6. **A changed logo.** Any change to the logo, or any use of it as part of
   your own branding.

## 6. Uses that are not allowed

1. **Imitating SWAT.** A website, application or service that looks like
   SWAT's own by its name, logo, colours or domain.
2. **Misrepresentation.** Any use that is false or misleading about who made
   a product, or that suggests a relationship with SWAT that does not exist.
3. **Registering the marks.** Registering a mark, or anything confusingly
   similar, as a trademark, company name or domain name of your own.
4. **Harmful content.** Using the marks with illegal, harmful or deceptive
   content, or content that disparages SWAT.

## 7. Enforcement and changes

1. **Report misuse** through the contact in section 8.
2. **Permission can be withdrawn.** SWAT may withdraw a permission if a use
   no longer follows this policy or could harm the marks.
3. **Changes to this policy.** SWAT may change this policy. The version on
   the repository's default branch is the current one. Significant changes
   are announced in the repository's changelog.

## 8. Contact

Questions and permission requests:
https://www.swatmobility.com/contact-us

---

The structure of this policy follows the OpenStreetMap Foundation Trademark
Policy. The rules for the logo on maps follow the map-logo practice of
commercial map platforms.
