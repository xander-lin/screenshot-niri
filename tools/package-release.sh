#!/usr/bin/env bash
#
# Deterministic, self-verifying release archives for the screenshot tools.
#
# Why this exists
# ---------------
# A release archive's sha256 is what the AUR PKGBUILD pins, so the bytes must
# be reproducible: packing the same binaries twice, on any day, has to yield
# the same file. A plain `tar czf` does not — it stamps the current time into
# the gzip header (and gzip also stores its own mtime and the member name), so
# two packs of identical content have different hashes. That exact mismatch
# shipped 0.2.2's first AUR upload (see found.md, lesson 44).
#
# Every source of variance is pinned:
#   --sort=name            file order is not readdir order
#   --mtime=@0             all member timestamps are epoch
#   --owner=0 --group=0    no builder identity
#   --numeric-owner        no name lookup
#   gzip -n                no gzip mtime, no member name
#
# The 0.2.2 release then shipped a second, worse mistake: the three
# `screenshot-niri` assets carried the `plain` variant's binary (byte-identical
# to the `screenshot-plain` assets) because the packer reads
# `target/*/release/screenshot` from whatever working tree was checked out at
# pack time, while README.md/LICENSE are copied from that same tree. The version
# string matched, so nothing complained (found.md, lesson 45). The packer now
# refuses that combination instead of trusting it:
#
#   * the checkout must be the branch the package names (or the exact release
#     tag of it) and must have no modified tracked file — README/LICENSE are
#     packed from this tree, so a different tree is a different package;
#   * each binary must be an ELF of the architecture its label claims;
#   * each binary is probed, not assumed: `--version` must print the version
#     being released, and `--help` must show the variant's own feature markers
#     and none of the other variant's (a `plain` binary cannot pass as `niri`);
#   * binaries must be newer than the newest tracked source file, so a stale
#     artifact from an earlier checkout cannot be packed;
#   * the same binary may not be used for two labels (x86-64-v3 vs generic);
#   * every archive is unpacked again and re-probed, and its README/LICENSE are
#     compared byte for byte against the tree, so a packing bug cannot hide
#     between the probe and the tarball.
#
# Usage
#   tools/package-release.sh --pkg screenshot-niri --version 0.2.2 \
#       --x86_64 target/release/screenshot \
#       --generic target-generic/release/screenshot \
#       --aarch64 target/aarch64-unknown-linux-gnu/release/screenshot \
#       --out dist
#
#   tools/package-release.sh --check-repro ...   # pack twice, assert equal
#   tools/package-release.sh --pkg ... --version ... --x86_64 ... \\
#       --verify dist                            # re-download and compare
#   tools/package-release.sh --variant plain ... # default: from --pkg name
#
# --verify needs `gh` authenticated and the Release to exist; it is the only
# mode that proves the AUR's pinned hashes match what users actually download.
# Do not pin a hash from a locally built archive: pin the hash of the file as
# published (download it, hash it, then fill the PKGBUILD).

set -euo pipefail

pkg=""
version=""
variant=""
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
	--variant)    variant="$2"; shift 2 ;;
	--x86_64)     bins[x86_64]="$2"; shift 2 ;;
	--generic)    bins[x86_64-generic]="$2"; shift 2 ;;
	--aarch64)    bins[aarch64]="$2"; shift 2 ;;
	--out)        out="$2"; shift 2 ;;
	--repo)       repo="$2"; shift 2 ;;
	--release)    release="$2"; shift 2 ;;
	--check-repro) check_repro=1; shift ;;
	--verify)     verify=1; out="$2"; shift 2 ;;
	-h|--help)    sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
	*)            die "unknown argument: $1" ;;
	esac
done

[[ -n $pkg ]]     || die "--pkg is required (e.g. screenshot-niri)"
[[ -n $version ]] || die "--version is required (e.g. 0.2.2)"
((${#bins[@]}))   || die "at least one of --x86_64/--generic/--aarch64 is required"

# The package name carries the variant: screenshot-niri / screenshot-plain. The
# variant is what the two branches are called, and it decides which feature
# markers the binaries must show.
variant="${variant:-${pkg#screenshot-}}"
case "$variant" in
niri | plain) ;;
*) die "cannot infer a known variant from --pkg $pkg; pass --variant niri|plain" ;;
esac
[[ ${pkg} == screenshot-${variant} ]] ||
	die "--pkg $pkg does not match --variant $variant"

# `--repo`/`--release` default from the package name: screenshot-niri -> the
# GitHub release v<version>-niri. Both can be overridden.
repo="${repo:-$(git remote get-url origin 2>/dev/null | sed -E 's#^.*github\.com[:/]##; s#\.git$##')}"
release="${release:-v${version}-${variant}}"

repo_root=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo_root"

# ------------------------------------------------------- what a variant is
# The help text is the honest contract a user reads, and it is what the 0.2.2
# mix-up made contradictory: the niri package must advertise long capture, the
# plain one must not. Markers are matched against `--help` output.
require_markers=()
forbid_markers=()
case "$variant" in
niri)
	require_markers=("--long" "long/scroll capture" "Toggle selection-box")
	forbid_markers=("region and full-screen capture")
	;;
plain)
	require_markers=("region and full-screen capture")
	forbid_markers=("--long" "--fps" "Toggle selection-box" "long/scroll capture")
	;;
esac

# ------------------------------------------------------------ source check
# README.md and LICENSE are packed from this checkout, so the checkout must be
# the one the package names. A detached HEAD is accepted only at the exact
# release tag, which is how a release is normally cut.
check_source() {
	local branch tag dirty
	branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null) || die "not a git checkout: $repo_root"
	tag=$(git describe --tags --exact-match HEAD 2>/dev/null || true)
	if [[ $branch != "$variant" && $tag != "v${version}-${variant}" ]]; then
		die "current checkout is '${branch:-detached}' (tag '${tag:-none}'), but $pkg must be packed from branch '$variant' or tag 'v${version}-${variant}'.
       README.md/LICENSE come from this tree while the binaries may come from anywhere, which is exactly how 0.2.2 shipped a plain binary in the niri package."
	fi
	# Only the files that end up in the archive matter here; a dirty src/ is
	# fine because the binaries are the source of truth (that is what the
	# freshness check is for).
	dirty=$(git status --porcelain --untracked-files=no -- README.md LICENSE)
	if [[ -n $dirty ]]; then
		printf '%s\n' "$dirty" >&2
		die "README.md/LICENSE are modified; they are packed verbatim, so commit them first"
	fi
	git diff --quiet HEAD -- 2>/dev/null ||
		note "warning: other tracked files are modified; they are not packed"
	note "source: branch '${branch}' at $(git rev-parse --short HEAD)${tag:+ (tag $tag)}, README/LICENSE unmodified"
}

# ------------------------------------------------------------- ELF helpers
elf_machine() {
	readelf -h "$1" 2>/dev/null | sed -n 's/^.*Machine:[[:space:]]*//p'
}

host_machine() {
	case "$(uname -m)" in
	x86_64) printf '%s' "Advanced Micro Devices X86-64" ;;
	aarch64) printf '%s' "AArch64" ;;
	*) printf '%s' "unknown" ;;
	esac
}

expected_machine() {
	case "$1" in
	x86_64 | x86_64-generic) printf '%s' "Advanced Micro Devices X86-64" ;;
	aarch64) printf '%s' "AArch64" ;;
	*) die "unknown label: $1" ;;
	esac
}

# --------------------------------------------------------------- probing
# probe_binary <label> <path>
# A binary is what the release promises only if it runs and says so. On a
# foreign architecture it cannot be executed here, so the same markers are read
# out of the file's strings and the check is reported as static.
probe_binary() {
	local label="$1" path="$2" machine host out marker ok=1
	[[ -f $path ]] || die "missing binary for $label: $path"
	[[ -x $path ]] || die "binary for $label is not executable: $path"
	machine=$(elf_machine "$path")
	[[ -n $machine ]] || die "not an ELF file: $path"
	[[ $machine == "$(expected_machine "$label")" ]] ||
		die "$label binary is '${machine}', expected '$(expected_machine "$label")': $path"
	host=$(host_machine)

	if [[ $machine == "$host" ]]; then
		out=$("$path" --version 2>&1) || die "$path --version failed: $out"
		[[ $out == "screenshot ${version}" ]] ||
			die "$path reports '${out}', release is ${version} (a stale build?)"
		# `--help` exits 0 and is the same text the user will see.
		out=$("$path" --help 2>&1) || die "$path --help failed"
		for marker in "${require_markers[@]}"; do
			[[ $out == *"$marker"* ]] || {
				printf '  missing marker %q in %s --help\n' "$marker" "$label" >&2
				ok=0
			}
		done
		for marker in "${forbid_markers[@]}"; do
			[[ $out != *"$marker"* ]] || {
				printf '  forbidden marker %q in %s --help\n' "$marker" "$label" >&2
				ok=0
			}
		done
		((ok)) || die "$label binary does not look like a '$variant' build: a build of the other branch?"
		note "probe $label: runs, version ${version}, '$variant' markers present"
	else
		out=$(strings -a "$path")
		for marker in "${require_markers[@]}"; do
			[[ $out == *"$marker"* ]] || {
				printf '  missing marker %q in %s\n' "$marker" "$label" >&2
				ok=0
			}
		done
		for marker in "${forbid_markers[@]}"; do
			[[ $out != *"$marker"* ]] || {
				printf '  forbidden marker %q in %s\n' "$marker" "$label" >&2
				ok=0
			}
		done
		((ok)) || die "$label binary does not look like a '$variant' build: a build of the other branch?"
		note "probe $label: ${machine}, static markers only (cannot execute a foreign architecture)"
	fi
}

# A binary older than the newest tracked source was built from another tree
# state; cargo would have rewritten it if it had been built from this one.
check_freshness() {
	local newest path bin newest_src
	newest=0
	newest_src=""
	while IFS= read -r -d '' path; do
		local m
		m=$(stat -c %Y -- "$path")
		if ((m > newest)); then
			newest=$m
			newest_src=$path
		fi
	done < <(git ls-files -z -- src Cargo.toml Cargo.lock)
	for bin in "${bins[@]}"; do
		local bm
		bm=$(stat -c %Y -- "$bin")
		if ((bm < newest)); then
			die "$bin is older than $newest_src — it was not built from this tree (rebuild it)"
		fi
	done
	note "freshness: every binary is newer than $(basename "$newest_src")"
}

# The same bytes must not stand in for two labels: x86-64-v3 and generic are
# different builds, and a release that ships one twice is a mistake.
check_distinct() {
	local a b
	for a in "${!bins[@]}"; do
		for b in "${!bins[@]}"; do
			[[ $a < $b ]] || continue
			if cmp -s "${bins[$a]}" "${bins[$b]}"; then
				die "labels $a and $b are the same binary: ${bins[$a]} vs ${bins[$b]}
       (build each architecture/CPU baseline into its own target dir)"
			fi
		done
	done
}

check_source
for label in "${!bins[@]}"; do
	probe_binary "$label" "${bins[$label]}"
done
check_freshness
check_distinct

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

# ------------------------------------------------ audit what actually shipped
# The probe above checks the input; this checks the file a user downloads. Both
# are needed: the 0.2.2 mistake produced a valid-looking tarball from the wrong
# input, and a packing bug can do the reverse.
audit_archive() {
	local label="$1" archive="$2"
	local dir root
	dir=$(mktemp -d)
	bsdtar -xf "$archive" -C "$dir" || die "cannot unpack $archive"
	root="${pkg}-${version}"
	[[ -f $dir/$root/screenshot ]] || die "$archive does not contain $root/screenshot"
	probe_binary "$label" "$dir/$root/screenshot"
	cmp -s "$dir/$root/README.md" README.md ||
		die "$archive carries a different README.md than this checkout ($variant)"
	cmp -s "$dir/$root/LICENSE" LICENSE ||
		die "$archive carries a different LICENSE than this checkout"
	rm -rf "$dir"
	note "audit $archive: unpacked binary re-probed, README/LICENSE match this checkout"
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
	audit_archive "$label" "$out/$name"
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

  gh release upload ${release} $out/*.tar.gz --clobber
  tools/package-release.sh --pkg $pkg --version $version \\
      --x86_64 ... --verify $out
EOF
