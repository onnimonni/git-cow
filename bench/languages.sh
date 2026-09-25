#!/usr/bin/env bash
# Compare `git worktree add` + setup/build against the git-cow wrapper, per language.
# Each language runs in a `nix shell` with its toolchain. Needs network (dependencies)
# and a release build (`cargo build --release`).
#
#   bench/languages.sh [lang...]      default: all
#
# Per language: build a small project with a dependency in `repo`, then
#   plain: GIT_COW_DISABLE=1 git worktree add + setup + build
#   cow:   git worktree add (wrapper, carries ignored build caches) + setup + build
#   check: change the printed marker in the cow worktree and rebuild; the program must
#          print the new marker and no file of the source worktree may change (catches
#          build caches with absolute paths that would build into the source checkout)
set -uo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
all_langs=(rust go python node elixir maven gradle cmake dotnet ruby php zig)

# --- per language: packages, init (in empty repo dir), build, run -------------------

rust_pkgs="cargo rustc"
rust_init() {
  cargo init -q --vcs none --name app . && cargo add -q serde_json
  printf 'fn main() { let v = serde_json::json!("hello-original"); println!("{}", v.as_str().unwrap()); }\n' > src/main.rs
  echo "target/" > .gitignore
}
rust_build() { cargo build -q; }
rust_run() { ./target/debug/app; }
rust_src=src/main.rs

go_pkgs="go"
go_init() {
  go mod init -q example.com/app 2>/dev/null || go mod init example.com/app
  printf 'package main\nimport ("fmt"; "github.com/google/uuid")\nfunc main() { _ = uuid.New(); fmt.Println("hello-original") }\n' > main.go
  go mod tidy
  echo "bin/" > .gitignore
}
go_build() { go build -o bin/app .; }
go_run() { ./bin/app; }
go_src=main.go

python_pkgs="uv python3"
python_init() {
  uv init -q --app --no-workspace --name app .
  uv add -q requests
  printf 'import requests\nprint("hello-original")\n' > main.py
}
python_build() { uv sync -q; }
python_run() { uv run -q python main.py; }
python_src=main.py

node_pkgs="nodejs"
node_init() {
  npm init -y >/dev/null
  npm install -s --no-audit --no-fund express@4 typescript@5 @types/express @types/node
  printf '{"compilerOptions":{"outDir":"dist","module":"commonjs","target":"es2022","esModuleInterop":true}}\n' > tsconfig.json
  mkdir -p src && printf 'import express from "express";\nvoid express;\nconsole.log("hello-original");\n' > src/index.ts
  printf 'node_modules/\ndist/\n' > .gitignore
}
node_build() { npm install -s --no-audit --no-fund && npx tsc; }
node_run() { node dist/index.js; }
node_src=src/index.ts

elixir_pkgs="elixir"
elixir_init() {
  mix new app >/dev/null && mv app/* app/.gitignore app/.formatter.exs . && rmdir app
  sed -i.bak 's/# {:dep_from_hexpm, "~> 0.3.0"}/{:jason, "~> 1.4"}, {:phoenix, "~> 1.7"}/' mix.exs && rm mix.exs.bak
  printf 'defmodule App do\n  def hello, do: IO.puts("hello-original")\nend\n' > lib/app.ex
}
elixir_build() { mix deps.get >/dev/null && mix compile >/dev/null; }
elixir_run() { mix run -e 'App.hello()'; }
elixir_src=lib/app.ex

maven_pkgs="maven jdk"
maven_init() {
  cat > pom.xml <<'EOF'
<project xmlns="http://maven.apache.org/POM/4.0.0"><modelVersion>4.0.0</modelVersion>
  <groupId>app</groupId><artifactId>app</artifactId><version>1.0</version>
  <properties><maven.compiler.release>17</maven.compiler.release>
    <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding></properties>
  <dependencies><dependency><groupId>com.google.code.gson</groupId><artifactId>gson</artifactId>
    <version>2.11.0</version></dependency></dependencies>
</project>
EOF
  mkdir -p src/main/java/app
  printf 'package app;\npublic class App { public static void main(String[] a) { System.out.println(new com.google.gson.Gson().fromJson("\\"hello-original\\"", String.class)); } }\n' > src/main/java/app/App.java
  echo "target/" > .gitignore
}
maven_build() { mvn -q package -DskipTests; }
maven_run() { java -cp "target/classes:$(find ~/.m2 -name 'gson-2.11.0.jar' | head -1)" app.App; }
maven_src=src/main/java/app/App.java

gradle_pkgs="gradle jdk"
gradle_init() {
  printf 'rootProject.name = "app"\n' > settings.gradle.kts
  printf 'plugins { application }\nrepositories { mavenCentral() }\ndependencies { implementation("com.google.code.gson:gson:2.11.0") }\napplication { mainClass.set("app.App") }\n' > build.gradle.kts
  mkdir -p src/main/java/app
  printf 'package app;\npublic class App { public static void main(String[] a) { System.out.println(new com.google.gson.Gson().fromJson("\\"hello-original\\"", String.class)); } }\n' > src/main/java/app/App.java
  printf 'build/\n.gradle/\n' > .gitignore
}
gradle_build() { gradle -q installDist; }
gradle_run() { ./build/install/app/bin/app; }
gradle_src=src/main/java/app/App.java

cmake_pkgs="cmake ninja"
cmake_init() {
  printf 'cmake_minimum_required(VERSION 3.20)\nproject(app C)\nadd_executable(app main.c)\n' > CMakeLists.txt
  printf '#include <stdio.h>\nint main(void) { puts("hello-original"); return 0; }\n' > main.c
  echo "build/" > .gitignore
}
cmake_build() { cmake -S . -B build -G Ninja >/dev/null && cmake --build build >/dev/null; }
cmake_run() { ./build/app; }
cmake_src=main.c

dotnet_pkgs="dotnet-sdk"
dotnet_init() {
  dotnet new console -n app -o . --force >/dev/null
  dotnet add package Newtonsoft.Json >/dev/null
  printf 'Console.WriteLine(Newtonsoft.Json.JsonConvert.DeserializeObject<string>("\\"hello-original\\""));\n' > Program.cs
  printf 'bin/\nobj/\n' > .gitignore
}
dotnet_build() { dotnet build -v q --nologo >/dev/null; }
dotnet_run() { dotnet bin/Debug/net*/app.dll; }
dotnet_src=Program.cs

ruby_pkgs="ruby"
ruby_init() {
  printf 'source "https://rubygems.org"\ngem "rack"\n' > Gemfile
  bundle config set --local path vendor/bundle >/dev/null
  bundle install --quiet
  printf 'require "rack"\nputs "hello-original"\n' > main.rb
  printf 'vendor/bundle/\n' > .gitignore
}
ruby_build() { bundle install --quiet; }
ruby_run() { bundle exec ruby main.rb; }
ruby_src=main.rb

php_pkgs="php phpPackages.composer"
php_init() {
  printf '{"require":{"monolog/monolog":"^3"}}\n' > composer.json
  composer install -q
  printf '<?php\nrequire __DIR__."/vendor/autoload.php";\n$l = new Monolog\\Logger("app");\necho "hello-original\\n";\n' > main.php
  echo "vendor/" > .gitignore
}
php_build() { composer install -q; }
php_run() { php main.php; }
php_src=main.php

zig_pkgs="zig"
zig_init() {
  zig init >/dev/null 2>&1
  printf 'const std = @import("std");\npub fn main() void { std.debug.print("hello-original\\n", .{}); }\n' > src/main.zig
  printf '.zig-cache/\nzig-out/\n' > .gitignore
}
zig_build() { zig build; }
zig_run() { ./zig-out/bin/* 2>&1; }
zig_src=src/main.zig

# --- harness -------------------------------------------------------------------------

now() { echo "$EPOCHREALTIME"; }
free_mb() { sync; df -m . | awk 'NR==2{print $4}'; }
# content hash of the source worktree, to prove the cow build didn't touch it
tree_hash() { (cd "$1" && find . -path ./.git -prune -o -type f -print0 | LC_ALL=C sort -z | xargs -0 cksum | cksum); }

inner() { # runs inside `nix shell` with the language toolchain
  local lang=$1 dir=$2
  export PATH="$root/wrapper:$root/target/release:$PATH"
  export GIT_CONFIG_GLOBAL=$dir/gitconfig GIT_CONFIG_NOSYSTEM=1 MIX_HOME=$dir/../.mix HEX_HOME=$dir/../.hex
  export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1 GRADLE_USER_HOME=$dir/../.gradle
  git config --global user.email bench@example.com; git config --global user.name bench
  git config --global commit.gpgsign false; git config --global init.defaultBranch main
  [[ $lang == elixir ]] && { mix local.hex --force >/dev/null; mix local.rebar --force >/dev/null; }
  cd "$dir" && mkdir repo && cd repo && git init -q
  "${lang}_init" >/dev/null 2>&1 || { echo "$lang: init failed"; return 1; }
  "${lang}_build" >/dev/null 2>&1 || { echo "$lang: build failed"; return 1; }
  git add -A && git commit -qm init
  local src=${lang}_src

  local a b t0 t1
  a=$(free_mb); t0=$(now)
  GIT_COW_DISABLE=1 git worktree add -q ../plain && (cd ../plain && "${lang}_build") >/dev/null 2>&1
  t1=$(now); b=$(free_mb)
  local plain_s plain_mb=$((a - b))
  plain_s=$(echo "$t1 - $t0" | bc)

  a=$(free_mb); t0=$(now)
  local summary
  summary=$(git worktree add ../cow 2>&1 >/dev/null) && (cd ../cow && "${lang}_build") >/dev/null 2>&1
  t1=$(now); b=$(free_mb)
  local cow_s cow_mb=$((a - b))
  cow_s=$(echo "$t1 - $t0" | bc)

  local before after out verdict=ok
  before=$(tree_hash .)
  sed -i.bak 's/hello-original/hello-cow/' "../cow/${!src}" && rm "../cow/${!src}.bak"
  if ! (cd ../cow && "${lang}_build") >/dev/null 2>&1; then
    verdict="rebuild failed"
  else
    out=$(cd ../cow && "${lang}_run" 2>&1)
    [[ $out == *hello-cow* ]] || verdict="stale output: ${out:0:60}"
  fi
  after=$(tree_hash .)
  [[ $before == "$after" ]] || verdict="SOURCE WORKTREE MODIFIED"
  local carried excluded
  carried=$(sed -n 's/.*; carried \([^;]*\).*/\1/p' <<<"$summary")
  excluded=$(sed -n 's/.*; excluded \([^;]*\).*/\1/p' <<<"$summary")
  printf '| %s | %.1f s, %s MB | %.1f s, %s MB | %s | %s | %s |\n' \
    "$lang" "$plain_s" "$plain_mb" "$cow_s" "$cow_mb" "$verdict" "${carried:--}" "${excluded:--}"
}

if [[ ${1:-} == --inner ]]; then
  inner "$2" "$3"
  exit
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for lang in "${@:-${all_langs[@]}}"; do
  pkgs_var=${lang}_pkgs
  pkgs=()
  for p in ${!pkgs_var} bash bc; do pkgs+=("nixpkgs#$p"); done
  mkdir -p "$work/$lang"
  nix shell "${pkgs[@]}" -c bash "$0" --inner "$lang" "$work/$lang" 2>&1 | grep -v 'evaluation warning'
done
