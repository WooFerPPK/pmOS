import { Terminal } from './modules/Terminal/Terminal.js';
import { InteractiveWindows } from './modules/InteractiveWindows/InteractiveWindows.js';
import { Observable } from './modules/observable/Observable.js';  // Import Observable
import AutoStartupFactory from '/assets/js/factories/AutoStartupFactory.js';
import { IconFactory } from './factories/IconFactory.js';
// import { WindowFactory } from './factories/WindowFactory.js';

// Create a shared Observable instance for the entire app context
const observable = new Observable();
let interactiveWindows = new InteractiveWindows({observable});

const iconFactory = new IconFactory(observable, interactiveWindows);
iconFactory.initialize();

const autoStartupFactory = new AutoStartupFactory(observable, interactiveWindows);
autoStartupFactory.initialize();
