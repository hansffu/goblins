{ lib }:
let
  fail = message: throw "mkGoblin: ${message}";
  validName = name: builtins.match "[A-Za-z_][A-Za-z0-9_-]*" name != null;
  validScope = name: builtins.isString name && builtins.stringLength name <= 64 && validName name;
  scopes = names: builtins.isList names && builtins.all validScope names && lib.unique names == names;
  validEtcPath =
    path:
    builtins.isString path
    && builtins.all (
      part: part != "." && part != ".." && builtins.match "[A-Za-z0-9_.-]+" part != null
    ) (lib.splitString "/" path);
  # Absolute, or beneath the sandbox home ($HOME, ${HOME} or ~), which is the
  # host user's home path.
  mountPath =
    path:
    builtins.isString path
    && builtins.match "(~|\\$HOME|\\$\\{HOME})?(/[A-Za-z0-9_.@+-]+)+" path != null
    && builtins.all (part: part != "." && part != "..") (lib.splitString "/" path)
    && builtins.all (reserved: path != reserved && !lib.hasPrefix "${reserved}/" path) [
      "/nix"
      "/proc"
      "/dev"
      "/run/goblins"
    ];
  etcFiles =
    files:
    builtins.isAttrs files
    && builtins.all (
      name: validEtcPath name && (builtins.isString files.${name} || lib.isDerivation files.${name})
    ) (builtins.attrNames files);
  storageMounts =
    mounts:
    builtins.isAttrs mounts
    && builtins.all (name: validName name && mountPath mounts.${name}) (builtins.attrNames mounts)
    && lib.unique (builtins.attrValues mounts) == builtins.attrValues mounts;
  # {key} placeholders for recorded variables; {{ and }} are literal braces.
  resumeElement =
    element:
    builtins.isString element
    && builtins.match "([^{}]|\\{\\{|}}|\\{[a-z][a-z0-9_]{0,31}})*" element != null;
  defaultKeys = [
    "allowedPackages"
    "env"
    "rwDirs"
    "rwFiles"
    "roDirs"
    "roFiles"
    "injectedFiles"
    "scopeStorage"
  ];
in
{
  inherit scopes validName storageMounts;
  scopeDefaults =
    defaults:
    lib.assertMsg (
      builtins.isAttrs defaults
      && builtins.all (name: builtins.elem name defaultKeys) (builtins.attrNames defaults)
    ) "mkScope: defaults accepts only ${lib.concatStringsSep ", " defaultKeys}";
  goblin =
    options:
    let
      inherit (options)
        binName
        outName
        description
        integration
        allowNix
        allowUnixSockets
        allowedDomains
        allowedHostPorts
        publishedPorts
        rwDirs
        rwFiles
        roDirs
        roFiles
        args
        resumeCommand
        injectedFiles
        devShellInjectedFiles
        env
        docker
        hostCommands
        scope
        allowedScopes
        scopeStorage
        allowedRunTargets
        bypassPermissions
        ;
    in
    if !validName binName || !validName outName then
      fail "binName and outName must be simple executable names"
    else if
      !builtins.isString description
      || description == ""
      || builtins.stringLength description > 256
      || lib.hasInfix "\n" description
      || lib.hasInfix "\r" description
    then
      fail "description must be a nonempty single-line string of at most 256 characters"
    else if
      integration != null
      && !builtins.elem integration [
        "codex"
        "claude"
      ]
    then
      fail "unsupported integration driver"
    else if docker ? defaultScope || docker ? allowedScopes then
      fail "docker.defaultScope and docker.allowedScopes were replaced by scope and allowedScopes; declare the scope with mkScope { docker.enable = true; }"
    else if
      !builtins.isAttrs docker
      || builtins.attrNames docker != [ "enable" ]
      || !builtins.isBool docker.enable
    then
      fail "docker accepts only enable (boolean)"
    else if
      !builtins.isAttrs hostCommands
      || builtins.attrNames hostCommands != [ "enable" ]
      || !builtins.isBool hostCommands.enable
    then
      fail "hostCommands accepts only enable (boolean)"
    else if !builtins.isBool bypassPermissions then
      fail "bypassPermissions must be a boolean"
    # Only an agent has permission prompts to bypass.
    else if bypassPermissions && integration == null then
      fail "bypassPermissions needs an agent goblin (mkClaudeGoblin or mkCodexGoblin)"
    else if !(scope == null || validScope scope) || !scopes allowedScopes then
      fail "scope must be null or a scope name, and allowedScopes a list of unique scope names"
    else if
      !(
        builtins.isList allowedRunTargets
        && builtins.all (name: builtins.isString name && validName name) allowedRunTargets
        && lib.unique allowedRunTargets == allowedRunTargets
      )
    then
      fail "allowedRunTargets must be a list of unique goblin names"
    else if !storageMounts scopeStorage then
      fail "scopeStorage must map storage names to distinct absolute or $HOME paths outside /nix, /proc, /dev and /run/goblins"
    else if !builtins.isBool allowNix then
      fail "allowNix must be a boolean"
    # The host Nix daemon fetches substitutes and fixed-output derivations over
    # the host's network, so an offline goblin with Nix would not be offline.
    else if allowNix && allowedDomains != null then
      fail "allowNix requires allowedDomains = null: the host Nix daemon downloads over the host network"
    else if allowUnixSockets != true then
      fail "allowUnixSockets must be true for the session request socket"
    else if
      (allowedDomains != null && allowedDomains != [ ])
      || allowedHostPorts != [ ]
      || publishedPorts != [ ]
    then
      fail "allowedDomains must be null (open) or [] (offline); domain filters and port mappings are not supported"
    else if
      !builtins.all (paths: builtins.isList paths && builtins.all builtins.isString paths) [
        rwDirs
        rwFiles
        roDirs
        roFiles
      ]
    then
      fail "rwDirs, rwFiles, roDirs and roFiles must be lists of path strings"
    else if !builtins.isList args || !builtins.all builtins.isString args then
      fail "args must be a list of strings"
    else if
      !(
        resumeCommand == null || builtins.isList resumeCommand && builtins.all resumeElement resumeCommand
      )
    then
      fail "resumeCommand must be null or a list of strings whose placeholders are {key} (key: a lowercase letter, then up to 31 lowercase letters, digits or _), with {{ and }} for literal braces"
    else if !etcFiles injectedFiles || !etcFiles devShellInjectedFiles then
      fail "injectedFiles and devShellInjectedFiles must map relative /etc file names to text or file derivations"
    else if
      lib.intersectLists (builtins.attrNames injectedFiles) (builtins.attrNames devShellInjectedFiles)
      != [ ]
    then
      fail "injectedFiles and devShellInjectedFiles cannot name the same file"
    else if
      !builtins.isAttrs env
      || !builtins.all (
        name: builtins.match "[A-Za-z_][A-Za-z0-9_]*" name != null && builtins.isString env.${name}
      ) (builtins.attrNames env)
    then
      fail "env must map environment variable names to literal strings"
    else if
      lib.intersectLists (builtins.attrNames env) [
        "PATH"
        "HOME"
        "SHELL"
        "USER"
        "LOGNAME"
        "PYTHONNOUSERSITE"
      ] != [ ]
    then
      fail "env cannot override PATH, HOME, SHELL, USER, LOGNAME or PYTHONNOUSERSITE"
    else
      true;

  goblins =
    goblins:
    if !builtins.isAttrs goblins || goblins == { } then
      throw "mkGoblins: goblins must be a nonempty attrset of mkGoblin results"
    else if !builtins.all validName (builtins.attrNames goblins) then
      throw "mkGoblins: goblin names must be simple identifiers"
    else
      true;
}
