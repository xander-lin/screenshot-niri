#!/usr/bin/env bash
#
# Deterministic release archives for the screenshot tools.
#
# Why this exists
# ---------------
# A release archive's sha256 is what the AUR PKGBUILD pins, so the bytes must
# be reproducible: packing the same binaries twice, on any day, has to yield
# the same file. A plain `tar czf` does not — it stamps the current time into
# the gzip header (and gzip also stores its own mtime and the member name), so
# two packs of identical content have different hashes. That exact mismatch
# shipped 0.2.2's first AUR upload (see found.md, lesson 9).
#
# Every source of variance is pinned:
#   --sort=name            file order is not readdir order
#   --mtime=@0             all member timestamps are epoch
#   --owner=0 --group=0    no builder identity
#   --numeric-owner        no name lookup
#   gzip -n                no gzip mtime, no member name
#
# Usage
#   tools/package-release.sh --pkg screenshot-niri --version 0.2.2 \
#       --x86_64 target/release/screenshot \
#       --generic target-generic/release/screenshot \
#       --aarch64 target/aarch64-unknown-linux-gnu/release/screenshot \
#       --out dist
#
#   tools/package-release.sh --pkg screenshot-niri --version 0.2.2 \\
#       --x86_64 target/release/screenshot ... --out dist
#   tools/package-release.sh --check-repro ...   # pack twice, assert equal
#   tools/package-release.sh --pkg ... --version ... --x86_64 ... \\
#       --verify dist                            # re-download and compare
#
# --verify needs `gh` authenticated and the Release to exist; it is the only
# mode that proves the AUR's pinned hashes match what users actually download.
# Do not pin a hash from a locally built archive: pin the hash of the file as
# published (download it, hash it, then fill the PKGBUILD).

set -euo pipefail

pkg=""
version=""
out="dist"
declare -A bins=()
check_repro=0
verify=0

die() { printf 'package-release: %s\n' "$*" >&2; exit 1; }
note() { printf '  %s\n' "$*"; }

while (($#)); do
	case "$1" in
	--pkg)        pkg="$2"; shift 2 ;;
	--version)    version="$2"; shift 2 ;;
	--x86_64)     bins[x86_64]="$2"; shift 2 ;;
	--generic)    bins[x86_64-generic]="$2"; shift 2 ;;
	--aarch64)    bins[aarch64]="$2"; shift 2 ;;
	--out)        out="$2"; shift 2 ;;
	--repo)       repo="$2"; shift 2 ;;
	--release)    release="$2"; shift 2 ;;
	--check-repro) check_repro=1; shift ;;
	--verify)     verify=1; shift 2 ;;
	-h|--help)    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
	*)            die "unknown argument: $1" ;;
	esac
done

[[ -n $pkg ]]     || die "--pkg is required (e.g. screenshot-niri)"
[[ -n $version ]] || die "--version is required (e.g. 0.2.2)"
((${#bins[@]}))   || die "at least one of --x86_64/--generic/--aarch64 is required"

# `--repo`/`--release` default from the package name: screenshot-niri -> the
# GitHub release v<version>-niri. Both can be overridden.
repo="${repo:-$(git remote get-url origin 2>/dev/null | sed -E 's#^.*github\.com[:/]##; s#\.git$##')}"
release="${release:-v${version}-${pkg#screenshot-}}"

repo_root=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo_root"

# ------------------------------------------------------------------- packing
# pack_one <label> <destination>
pack_one() {
	local label="$1" dest="$2"
	local src="${bins[$label]}"
	[[ -f $src ]] || die "missing binary for $label: $src"

	local stage root
	stage=$(mktemp -d)
	root="${pkg}-${version}"
	mkdir -p "$stage/$root"
	cp "$src" "$stage/$root/screenshot"
	cp LICENSE "$stage/$root/LICENSE"
	cp README.md "$stage/$root/README.md"
	chmod 755 "$stage/$root/screenshot"
	chmod 644 "$stage/$root/LICENSE" "$stage/$root/README.md"

	# The determinism flags, in one place. See the header for why each is here.
	tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
		--format=gnu -cf - -C "$stage" "$root" | gzip -9n > "$dest"

	rm -rf "$stage"
}

# ---------------------------------------------------------------- verify mode
if ((verify)); then
	mkdir -p "$out"
	fail=0
	for label in "${!bins[@]}"; do
		name="${pkg}-${version}-${label}.tar.gz"
		curl_ok=0
		if [[ -f $out/$name ]]; then
			note "cached: $name"
		else
			gh release download "$release" --repo "$repo" \
				-p "$name" -D "$out" >/dev/null || die "could not download $name (is $release published?)"
		fi
		published=$(sha256sum "$out/$name" | cut -d' ' -f1)
		local_file="$out/$name.local"
		pack_one "$label" "$local_file"
		local_sum=$(sha256sum "$local_file" | cut -d' ' -f1)
		if [[ $published == "$local_sum" ]]; then
			note "OK      $name  $published"
		else
			printf '  MISMATCH %s\n    published %s\n    local     %s\n' \
				"$name" "$published" "$local_sum" >&2
			fail=1
		fi
		rm -f "$local_file"
	done
	exit $fail
fi

mkdir -p "$out"
names=()
for label in x86_64 x86_64-generic aarch64; do
	[[ -n ${bins[$label]:-} ]] || continue
	name="${pkg}-${version}-${label}.tar.gz"
	pack_one "$label" "$out/$name"
	names+=("$name")
done

printf '%s\n' "packed ${#names[@]} archive(s) into $out:"
for name in "${names[@]}"; do
	printf '  %s  %s\n' "$(sha256sum "$out/$name" | cut -d' ' -f1)" "$name"
done

# --------------------------------------------------------------- repro check
if ((check_repro)); then
	printf '%s\n' "reproducibility check (second pack, must be byte-identical):"
	for name in "${names[@]}"; do
		label=${name#"${pkg}-${version}-"}
		label=${label%.tar.gz}
		pack_one "$label" "$out/.repro-$name"
		if cmp -s "$out/$name" "$out/.repro-$name"; then
			note "OK  $name"
		else
			die "NOT REPRODUCIBLE: $name differs between two packs"
		fi
		rm -f "$out/.repro-$name"
	done
fi

cat <<-EOF

Next: upload, then pin the hashes of the files AS PUBLISHED —
download them and hash those, never the local copies:

  gh release upload v${version}-NIRI $out/*.tar.gz --clobber
  tools/package-release.sh --pkg $pkg --version $version \\
      --x86_64 ... --verify $out
EOF
