"""Package Expo's public fonts/icons outside ignored dependency paths, then rehash entries.

Only asset URLs and whitespace-only lines in third-party license comments are changed.
Wallet/signing code and license text are preserved.
"""
import hashlib
from pathlib import Path
import re
import shutil
import sys


def normalize(directory):
    root = Path(directory).resolve()
    chunks = root / '_expo/static/js/web'
    if not chunks.is_dir():
        raise ValueError('Expected an Expo web export')
    moves, prefixes = [], set()
    for path in (root / 'assets').rglob('*'):
        if not path.is_file() or 'node_modules' not in path.relative_to(root).parts:
            continue
        parts = path.relative_to(root).parts
        index = parts.index('node_modules')
        if path.is_symlink() or path.suffix not in ['.ttf', '.otf', '.png', '.webp', '.jpg', '.jpeg', '.svg', '.gif']:
            raise ValueError('Only exported public fonts and images may be relocated')
        target = root / 'assets/vendor' / Path(*parts[index + 1:])
        if 'node_modules' in target.relative_to(root).parts:
            raise ValueError('Nested dependency path needs inspection')
        if target.exists() and target.read_bytes() != path.read_bytes():
            raise ValueError('Exported asset name collision')
        prefixes.add(Path(*parts[:index + 1]).as_posix())
        moves.append((path, target))
    changed = {}
    for path in root.rglob('*'):
        if path.suffix not in ['.js', '.html', '.json', '.css']:
            continue
        original = path.read_text()
        cleaned = original
        for prefix in prefixes:
            cleaned = cleaned.replace(prefix, 'assets/vendor')
        if path.suffix == '.js':
            cleaned = re.sub(r'/\*![\s\S]*?\*/', lambda m: re.sub(r'(?m)^[ \t]+$', '', m[0]), cleaned)
        if cleaned != original:
            changed[path] = cleaned
    replacements = []
    for path, cleaned in changed.items():
        if path.suffix != '.js':
            continue
        match = re.fullmatch(r'(.+)-[a-f0-9]{32}\.js', path.name)
        if not match:
            raise ValueError('Expected a content-hashed JS entry')
        # Entry bundles are referenced only by HTML; refuse to alter a dependency graph.
        if any(path.name in other.read_text() for other in chunks.glob('*.js') if other != path):
            raise ValueError('A changed asset belongs to a shared chunk; inspect the export')
        name = match[1] + '-' + hashlib.md5(cleaned.encode()).hexdigest() + '.js'
        if path.with_name(name).exists():
            raise ValueError('The normalized entry already exists')
        replacements.append((path, name))
    for source, target in moves:
        assert source.resolve().is_relative_to(root / 'assets')
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
        source.unlink()
    for path, text in changed.items():
        path.write_text(text)
    for path, name in replacements:
        path.rename(path.with_name(name))
        for html in root.rglob('*.html'):
            content = html.read_text()
            if path.name in content:
                html.write_text(content.replace(path.name, name))
    return len(moves), len(replacements)


if __name__ == '__main__':
    assets, entries = normalize(sys.argv[1])
    print('Packaged public font/image assets:', assets, '; rehashed entries:', entries)
