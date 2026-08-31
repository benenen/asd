; Inno Setup script for the asd Windows installer.
;
; Driven by .github/workflows/windows-installer.yml, which stages the payload
; and passes the details on the command line:
;
;   iscc /DAppVersion=0.1.7 /DPayloadDir=<abs path> /DOutputBase=asd-0.1.7-x86_64-setup \
;        /DBuildFlavor=full deploy\windows\asd.iss
;
; Building it by hand works the same way; only AppVersion/PayloadDir/OutputBase
; are required (each has a placeholder default so `iscc` at least parses).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef PayloadDir
  #define PayloadDir "stage"
#endif
#ifndef OutputBase
  #define OutputBase "asd-setup"
#endif
#ifndef BuildFlavor
  #define BuildFlavor "full"
#endif

; ArchitecturesAllowed=x64compatible below needs 6.3+; on an older toolset it
; would be an unrecognized value, so fail with the reason instead.
#if VER < EncodeVer(6,3,0)
  #error Inno Setup 6.3 or newer is required
#endif

#define AppName "asd"
#define AppPublisher "benshi"
#define AppExeName "asd.exe"
#define AppUrl "https://github.com/benenen/asd"

[Setup]
; Stable AppId: this is what makes a later installer *upgrade* an existing
; install instead of stacking a second copy beside it. Never change it.
AppId={{7C3F5B92-1D64-4E8A-9B2F-0A6E5C1D8F34}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion} ({#BuildFlavor})
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}
AppUpdatesURL={#AppUrl}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
LicenseFile={#PayloadDir}\LICENSE
OutputDir=.
OutputBaseFilename={#OutputBase}
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Program Files and the machine PATH both need elevation.
PrivilegesRequired=admin
ChangesEnvironment=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#AppExeName}

[Tasks]
Name: "addtopath"; Description: "Add asd to the system PATH"; GroupDescription: "Integration:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Integration:"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
; Every flavor links asd-vt and imports this, and the build stages it; the flag
; only tolerates a hand-assembled payload that left it out.
Source: "{#PayloadDir}\ghostty-vt.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#PayloadDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#PayloadDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
; Bare `asd` opens the GUI, so a shortcut is meaningful for every flavor that
; includes it; for a CLI-only build it simply starts the terminal client.
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Registry]
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; \
    ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; \
    Check: NeedsAddPath(ExpandConstant('{app}')); Tasks: addtopath

[Code]
const
  EnvKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

// True when {app} is not already a PATH entry. Compared with separators on both
// sides so "C:\Program Files\asd" does not match "C:\Program Files\asdx".
//
// Line comments, not the `{ ... }` form: Pascal comments do not nest, so an
// Inno constant like {app} inside one closes it early and the rest of the
// sentence is compiled as code.
function NeedsAddPath(Param: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE, EnvKey, 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

// Uninstall leaves no stale PATH entry behind. The list is split on ';' and
// rebuilt without our entry, rather than cut out by index — that way first,
// last and only-entry all fall out correctly, and every other entry is written
// back exactly as it was.
procedure RemoveFromPath(Param: string);
var
  OrigPath, NewPath, Item: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE, EnvKey, 'Path', OrigPath) then
    exit;
  NewPath := '';
  while Length(OrigPath) > 0 do
  begin
    P := Pos(';', OrigPath);
    if P > 0 then
    begin
      Item := Copy(OrigPath, 1, P - 1);
      Delete(OrigPath, 1, P);
    end
    else
    begin
      Item := OrigPath;
      OrigPath := '';
    end;
    if (Item <> '') and (CompareText(Item, Param) <> 0) then
    begin
      if NewPath <> '' then
        NewPath := NewPath + ';';
      NewPath := NewPath + Item;
    end;
  end;
  RegWriteExpandStringValue(HKEY_LOCAL_MACHINE, EnvKey, 'Path', NewPath);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
