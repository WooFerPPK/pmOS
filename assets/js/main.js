import { InteractiveWindows } from './modules/InteractiveWindows/InteractiveWindows.js';
import AutoStartupFactory from '/assets/js/factories/AutoStartupFactory.js';
import { IconFactory } from './factories/IconFactory.js';

// const observable = new Observable(); Not in use yet
let interactiveWindows = new InteractiveWindows();

const iconFactory = new IconFactory(interactiveWindows);
iconFactory.initialize();

const autoStartupFactory = new AutoStartupFactory(interactiveWindows);
autoStartupFactory.initialize();
