import { Observable } from './modules/Observable/Observable.js';
import { InteractiveWindows } from './modules/InteractiveWindows/InteractiveWindows.js';
import AutoStartupFactory from '/assets/js/factories/AutoStartupFactory.js';
import { IconFactory } from './factories/IconFactory.js';

const observable = new Observable();
let interactiveWindows = new InteractiveWindows({observable});

const iconFactory = new IconFactory(interactiveWindows, observable);
iconFactory.initialize();

const autoStartupFactory = new AutoStartupFactory(interactiveWindows, observable);
autoStartupFactory.initialize();
