.PHONY: all install compile package clean

VSCE := $(if $(shell command -v vsce 2>/dev/null),vsce,npx --yes @vscode/vsce)

all: package

install:
	npm install

compile: install
	npm run compile

package: compile
	$(VSCE) package --allow-missing-repository --skip-license

clean:
	rm -rf out *.vsix
