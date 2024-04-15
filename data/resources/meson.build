blueprints = custom_target('blueprints',
  input: files(
    'blueprints/help-overlay.blp',
    'blueprints/window.blp',
  ),
  output: '.',
  command: [find_program('blueprint-compiler'), 'batch-compile', '@OUTPUT@', '@CURRENT_SOURCE_DIR@', '@INPUT@'],
)

scalable_dir = 'icons' / 'hicolor' / 'scalable' / 'apps'
install_data(
  [
    scalable_dir / ('@0@.svg').format(base_id),
    scalable_dir / ('@0@.svg').format(application_id)
  ],
  install_dir: datadir /scalable_dir
)

symbolic_dir = 'icons' / 'hicolor' / 'symbolic' / 'apps'
install_data(
  symbolic_dir / ('@0@-symbolic.svg').format(base_id),
  install_dir: datadir / symbolic_dir
)

action_dir = 'icons' / 'hicolor' / 'scalable' / 'actions'
action_icons = [
    # each icon must be registered here
    action_dir / 'image-missing-symbolic.svg',
    action_dir / 'paper-symbolic.svg',
    action_dir / 'usb-stick-symbolic.svg',
    action_dir / 'check-round-outline-symbolic.svg',
    action_dir / 'view-refresh-symbolic.svg',
    action_dir / 'error-symbolic.svg',
    action_dir / 'flash-symbolic.svg',
]

install_data(
  action_icons,
  install_dir: pkgdatadir / action_dir
)

# Resources
resources = gnome.compile_resources(
    'resources',
    'resources.gresource.xml',
    gresource_bundle: true,
    source_dir: meson.current_build_dir(),
    install: true,
    install_dir: pkgdatadir,
    dependencies: blueprints,
)